//! Bounded console delivery. A slow journal/pipe or held stderr lock cannot stop producers.
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsoleStatus {
    pub written: u64,
    pub dropped: u64,
    pub failures: u64,
    pub last_error: Option<String>,
}
#[derive(Default)]
struct Counters {
    written: AtomicU64,
    dropped: AtomicU64,
    failures: AtomicU64,
    error: Mutex<Option<String>>,
}
enum Message {
    Line(String),
    Flush(mpsc::SyncSender<()>),
    Stop(mpsc::SyncSender<()>),
}
pub struct ConsoleSink {
    tx: Option<mpsc::SyncSender<Message>>,
    counters: Arc<Counters>,
}
impl ConsoleSink {
    pub fn stderr() -> Self {
        Self::start(Box::new(std::io::stderr()), 256)
    }
    fn start(mut writer: Box<dyn Write + Send>, capacity: usize) -> Self {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let counters = Arc::new(Counters::default());
        let state = counters.clone();
        let spawned = std::thread::Builder::new()
            .name("awcc-console".into())
            .spawn(move || {
                while let Ok(message) = rx.recv() {
                    match message {
                        Message::Line(line) => match writeln!(writer, "{line}") {
                            Ok(()) => {
                                state.written.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                state.failures.fetch_add(1, Ordering::Relaxed);
                                *state.error.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(e.to_string());
                            }
                        },
                        Message::Flush(done) => {
                            let _ = writer.flush();
                            let _ = done.try_send(());
                        }
                        Message::Stop(done) => {
                            let _ = writer.flush();
                            let _ = done.try_send(());
                            break;
                        }
                    }
                }
            });
        let tx = match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                *counters.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                counters.failures.fetch_add(1, Ordering::Relaxed);
                None
            }
        };
        Self { tx, counters }
    }
    pub fn send(&self, line: String) {
        if self
            .tx
            .as_ref()
            .is_none_or(|tx| tx.try_send(Message::Line(line)).is_err())
        {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn status(&self) -> ConsoleStatus {
        ConsoleStatus {
            written: self.counters.written.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
            failures: self.counters.failures.load(Ordering::Relaxed),
            last_error: self
                .counters
                .error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .as_ref()
            .is_some_and(|sender| sender.try_send(Message::Flush(tx)).is_ok())
            && rx.recv_timeout(timeout).is_ok()
    }
    pub fn shutdown(&self, timeout: Duration) {
        let (tx, rx) = mpsc::sync_channel(1);
        if self
            .tx
            .as_ref()
            .is_some_and(|sender| sender.try_send(Message::Stop(tx)).is_ok())
        {
            let _ = rx.recv_timeout(timeout);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    struct Blocked(mpsc::SyncSender<()>, mpsc::Receiver<()>);
    impl Write for Blocked {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.try_send(());
            let _ = self.1.recv();
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn blocked_console_and_full_queue_do_not_block_producers() {
        let (entered, waiting) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let sink = ConsoleSink::start(Box::new(Blocked(entered, gate)), 2);
        sink.send("first".into());
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            sink.send("queued".into());
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(sink.status().dropped > 0);
        assert!(!sink.flush(Duration::from_millis(20)));
        drop(release);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while sink.status().written < 3 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(sink.flush(Duration::from_secs(1)));
    }
}
