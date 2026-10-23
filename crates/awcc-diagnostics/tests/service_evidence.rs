#![allow(clippy::unwrap_used, clippy::expect_used)]
use awcc_diagnostics::service::{MAX_DAEMON_EVENTS, ServiceSupport};
use awcc_diagnostics::{
    DiagnosticsConfig, DiagnosticsRuntime, NewEvent, Severity, resolve_log_level,
};
use std::collections::BTreeMap;

fn runtime() -> std::sync::Arc<DiagnosticsRuntime> {
    let mut cfg = DiagnosticsConfig::from_env_and_cli(
        "test-service",
        resolve_log_level(None, false, None).unwrap(),
        false,
    );
    cfg.enable_file_sink = false;
    cfg.log_dir = None;
    cfg.enable_stderr_console = false;
    cfg.event_ring_capacity = 512;
    DiagnosticsRuntime::new(cfg)
}

#[test]
fn bounded_pages_preserve_recent_sequences_and_redact_before_crossing_ipc() {
    let rt = runtime();
    for n in 0..400 {
        let mut event = NewEvent::new(
            Severity::Info,
            "provider",
            format!(
                "event {n} /home/private-user/session contact=user@example.com {}",
                "x".repeat(5000)
            ),
        );
        for key in 0..32 {
            event.fields.insert(format!("field{key}"), "y".repeat(3000));
        }
        rt.record(event);
    }
    let mut cursor = 0;
    let mut seen = Vec::new();
    loop {
        let page = rt.daemon_page(cursor, 65535, serde_json::Value::Null);
        let encoded = serde_json::to_string(&page).unwrap();
        assert!(encoded.len() < 60 * 1024);
        assert!(!encoded.contains("private-user"));
        assert!(!encoded.contains("user@example.com"));
        assert!(page.events.len() <= 16);
        seen.extend(page.events.iter().map(|event| event.seq));
        if !page.has_more {
            break;
        }
        assert!(page.next_seq > cursor);
        cursor = page.next_seq;
    }
    assert_eq!(seen.len(), MAX_DAEMON_EVENTS);
    assert!(seen.windows(2).all(|seq| seq[1] == seq[0] + 1));
    assert_eq!(seen.last().copied(), Some(401));
}

#[test]
fn service_events_and_missing_evidence_round_trip_into_redacted_zip() {
    let rt = runtime();
    rt.record(NewEvent::new(
        Severity::Error,
        "power_bus",
        "polkit expected=(bba{ss}) received=(bba{sv}) /home/private-user/file",
    ));
    let mut bundle = rt.assemble_support_bundle(None, BTreeMap::new()).unwrap();
    let mut support = ServiceSupport {
        state: "available".into(),
        daemon: Some(rt.daemon_page(0, 16, serde_json::json!({"selected":"balanced"}))),
        ..Default::default()
    };
    support.context.insert(
        "context".into(),
        "contact=user@example.com /home/private-user/file".into(),
    );
    support
        .gaps
        .push("journal unavailable: permission denied".into());
    bundle.attach_service_support(support).unwrap();
    assert!(bundle.summary_txt.contains("permission denied"));
    assert!(bundle.report_json.contains("service_diagnostics"));
    assert!(!bundle.report_json.contains("private-user"));
    let zip = bundle.to_zip_bytes().unwrap();
    let raw = String::from_utf8_lossy(&zip);
    assert!(raw.contains("daemon-events.jsonl"));
    assert!(raw.contains("service-diagnostics.json"));
    assert!(raw.contains("expected=(bba{ss})"));
    assert!(!raw.contains("user@example.com"));
}

#[test]
fn oversized_cached_state_is_explicitly_omitted_to_keep_ipc_bounded() {
    let rt = runtime();
    let page = rt.daemon_page(0, 16, serde_json::json!({"unknown":"x".repeat(128*1024)}));
    assert_eq!(page.power["payload_truncated"], true);
    assert!(serde_json::to_vec(&page).unwrap().len() < 60 * 1024);
}
