//! Bounded diagnostic evidence shared across the authorized daemon IPC and support exporter.
use crate::{BuildIdentityReport, Event, SinkStatus, console::ConsoleStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_DAEMON_EVENTS: usize = 256;
pub const MAX_PAGE_EVENTS: usize = 16;
pub const MAX_PAGE_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub active: bool,
    pub remaining_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonDiagnostics {
    pub session_id: String,
    pub build: BuildIdentityReport,
    pub events: Vec<Event>,
    pub history: Vec<Event>,
    pub history_note: Option<String>,
    pub next_seq: u64,
    pub oldest_seq: Option<u64>,
    pub evicted: u64,
    pub cleared: u64,
    pub has_more: bool,
    pub recording: RecordingStatus,
    pub base_level: String,
    pub active_level: String,
    pub file_sink: SinkStatus,
    pub console: ConsoleStatus,
    /// Already-published policy outcomes only; never a hardware refresh.
    pub power: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSupport {
    pub state: String,
    pub daemon: Option<DaemonDiagnostics>,
    pub context: BTreeMap<String, String>,
    pub startup_journal: Vec<String>,
    pub gaps: Vec<String>,
}

impl crate::DiagnosticsRuntime {
    /// No interest notification, polling, transport construction, or hardware read is allowed here.
    pub fn daemon_page(
        &self,
        after: u64,
        limit: usize,
        power: serde_json::Value,
    ) -> DaemonDiagnostics {
        let (all, stats) = self.events_snapshot();
        let mut events = Vec::new();
        let mut bytes = 0;
        let mut next_seq = after;
        let mut has_more = false;
        let begin = if after == 0 {
            all.len().saturating_sub(MAX_DAEMON_EVENTS)
        } else {
            0
        };
        for event in all
            .iter()
            .skip(begin)
            .filter(|event| event.seq > after)
            .cloned()
        {
            let event = clean_event(event, &self.redactor);
            let size = serde_json::to_vec(&event).map_or(MAX_PAGE_BYTES, |bytes| bytes.len());
            if events.len() >= limit.clamp(1, MAX_PAGE_EVENTS) || bytes + size > MAX_PAGE_BYTES {
                has_more = true;
                break;
            }
            bytes += size;
            next_seq = event.seq;
            events.push(event);
        }
        let (history, history_note) = if after == 0 {
            let (past, stats) = crate::collect_and_merge_events_with_limits(
                self.log_dir.as_deref(),
                &[],
                128,
                4,
                8,
                256 * 1024,
            );
            let mut history = Vec::new();
            let mut bytes = 0;
            let mut truncated = false;
            for event in past
                .into_iter()
                .rev()
                .filter(|event| event.session_id != self.session_id)
            {
                let event = clean_event(event, &self.redactor);
                let size = serde_json::to_vec(&event).map_or(8192, |bytes| bytes.len());
                if bytes + size > 8192 {
                    truncated = true;
                    break;
                }
                bytes += size;
                history.push(event);
            }
            history.reverse();
            let note = if truncated {
                Some("Historical daemon/setup events limited to 8 KiB".into())
            } else {
                stats.note
            };
            (history, note)
        } else {
            (Vec::new(), None)
        };
        let mut power = power;
        self.redactor.redact_json(&mut power);
        if serde_json::to_vec(&power).map_or(true, |bytes| bytes.len() > 16 * 1024) {
            power = serde_json::json!({"unavailable":"cached power summary exceeded diagnostic IPC bound","payload_truncated":true});
        }
        let mut file_sink = self.sink_status();
        file_sink.directory = file_sink.directory.map(|value| {
            self.redactor
                .redact_text(&value)
                .chars()
                .take(256)
                .collect()
        });
        file_sink.active_file = file_sink.active_file.map(|value| {
            self.redactor
                .redact_text(&value)
                .chars()
                .take(256)
                .collect()
        });
        file_sink.last_error = file_sink.last_error.map(|value| {
            self.redactor
                .redact_text(&value)
                .chars()
                .take(256)
                .collect()
        });
        let mut console = self.console_status();
        console.last_error = console.last_error.map(|value| {
            self.redactor
                .redact_text(&value)
                .chars()
                .take(256)
                .collect()
        });
        DaemonDiagnostics {
            session_id: self.session_id.clone(),
            build: self.build_info.clone(),
            events,
            history,
            history_note,
            next_seq,
            oldest_seq: all.first().map(|event| event.seq),
            evicted: stats.evicted,
            cleared: stats.cleared,
            has_more,
            recording: self.recording_status(),
            base_level: self.base_level.as_str().into(),
            active_level: self.active_level().as_str().into(),
            file_sink,
            console,
            power,
        }
    }
    pub fn recording_status(&self) -> RecordingStatus {
        RecordingStatus {
            active: self.is_recording_active(),
            remaining_secs: self.recording_remaining_secs(),
        }
    }
}

impl crate::SupportBundle {
    pub fn attach_service_support(
        &mut self,
        evidence: ServiceSupport,
    ) -> Result<(), crate::ExportError> {
        let redactor = crate::Redactor::from_environment();
        let mut value = serde_json::to_value(evidence)?;
        redactor.redact_json(&mut value);
        let evidence: ServiceSupport = serde_json::from_value(value)?;
        self.summary_txt.push_str(&format!(
            "\nPowerProfiles service diagnostics: {}\n",
            evidence.state
        ));
        if let Some(daemon) = &evidence.daemon {
            self.summary_txt.push_str(&format!(
                "Daemon build: {}\nDaemon events: {} (evicted: {})\n",
                daemon.build.summary_line(),
                daemon.events.len() + daemon.history.len(),
                daemon.evicted
            ));
        }
        for gap in &evidence.gaps {
            self.summary_txt.push_str(&format!("Evidence gap: {gap}\n"));
        }
        self.report.service_diagnostics = Some(evidence);
        self.report_json = serde_json::to_string_pretty(&self.report)?;
        Ok(())
    }
}

fn clean_event(mut event: Event, redactor: &crate::Redactor) -> Event {
    let mut value = serde_json::to_value(&event).unwrap_or_default();
    redactor.redact_json(&mut value);
    match serde_json::from_value::<Event>(value) {
        Ok(clean) => event = clean,
        Err(_) => {
            event.message = "Event unavailable after redaction".into();
            event.fields.clear();
            event.action = None;
            event.outcome = None;
            event.code = None;
        }
    }
    let truncated = event.message.chars().count() > 1024
        || event.fields.len() > 16
        || event
            .fields
            .iter()
            .any(|(key, value)| key.chars().count() > 64 || value.chars().count() > 128);
    event.message = event.message.chars().take(1024).collect();
    event.session_id = event.session_id.chars().take(64).collect();
    event.ts_utc = event.ts_utc.chars().take(64).collect();
    event.component = event.component.chars().take(128).collect();
    event.code = event.code.map(|value| value.chars().take(64).collect());
    event.action = event.action.map(|value| value.chars().take(128).collect());
    event.outcome = event.outcome.map(|value| value.chars().take(128).collect());
    event.fields = event
        .fields
        .into_iter()
        .take(16)
        .map(|(key, value)| {
            (
                key.chars().take(64).collect(),
                value.chars().take(128).collect(),
            )
        })
        .collect();
    if truncated {
        event
            .fields
            .insert("payload_truncated".into(), "true".into());
    }
    event
}
