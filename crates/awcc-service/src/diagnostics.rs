//! Passive integration evidence. Fixed read-only tool arguments; no activation, elevation or polling.
use crate::{
    installer::{HostCommand, HostRunner, HostTool, SystemRunner},
    ipc::DaemonClient,
};
use awcc_diagnostics::service::{DaemonDiagnostics, MAX_DAEMON_EVENTS, ServiceSupport};
use std::{collections::BTreeMap, time::Duration};

pub fn short(text: &str) -> String {
    text.chars().take(128).collect()
}

pub fn power_summary(power: &crate::power::PowerStatus) -> serde_json::Value {
    let mappings: BTreeMap<_, _> = power
        .counterparts
        .iter()
        .map(|(profile, policy)| {
            let value = match policy {
                awcc_core::ThermalPolicy::Preset { mode } => mode.display_name(),
                awcc_core::ThermalPolicy::Manual { .. } => "Manual",
            };
            (profile.as_str(), value)
        })
        .collect();
    serde_json::json!({
        "selected": power.selected, "active": power.active, "available": power.available,
        "counterparts": mappings, "counterpart_details": power.counterparts, "generation": power.generation,
        "cpu_confirmed": power.cpu.confirmed,
        "cpu_drivers": power.cpu.drivers.iter().take(8).map(|v| short(v)).collect::<Vec<_>>(),
        "cpu_controls": power.cpu.controls.iter().take(8).map(|v| short(v)).collect::<Vec<_>>(),
        "cpu_errors": power.cpu.errors.iter().take(4).map(|v| short(v)).collect::<Vec<_>>(),
        "thermal_outcome": power.thermal.as_ref().map(|v| short(&v.detail_message())),
        "problem": power.problem.as_deref().map(short),
        "hold_count": power.holds.len(),
        "holds": power.holds.iter().take(4).map(|h| serde_json::json!({"profile":h.profile,"application_id":short(&h.application_id),"reason":short(&h.reason)})).collect::<Vec<_>>()
    })
}

fn read_tool(runner: &dyn HostRunner, tool: HostTool, args: &[&str]) -> Result<String, String> {
    let output = runner
        .run(
            &HostCommand::new(tool, args.iter().copied()).with_timeout(Duration::from_secs(2)),
            None,
        )
        .map_err(|error| {
            short(&awcc_diagnostics::Redactor::from_environment().redact_text(&error.to_string()))
        })?;
    let redactor = awcc_diagnostics::Redactor::from_environment();
    let stdout = redactor.redact_text(&output.stdout);
    let stderr = redactor.redact_text(&output.stderr);
    if output.success() {
        let limit = if tool == HostTool::Journalctl {
            16384
        } else {
            4096
        };
        let mut text: String = if tool == HostTool::Journalctl {
            stdout
                .chars()
                .rev()
                .take(limit)
                .collect::<String>()
                .chars()
                .rev()
                .collect()
        } else {
            stdout.chars().take(limit).collect()
        };
        let version_probe = matches!(
            tool,
            HostTool::DbusBroker
                | HostTool::DbusDaemon
                | HostTool::PlasmaShell
                | HostTool::GnomeShell
        );
        if version_probe && stdout.trim().is_empty() {
            return Err("version probe returned no version output".into());
        }
        if !version_probe && !stderr.trim().is_empty() {
            text.push_str(&format!("\n[diagnostic warning: {}]", short(&stderr)));
        }
        if stdout.chars().count() > limit {
            text.push_str("\n[evidence truncated]");
        }
        Ok(text)
    } else {
        Err(short(&stderr))
    }
}

/// Query only published bus properties and systemd state. Missing tools/services remain explicit.
pub fn integration_context(
    runner: &dyn HostRunner,
    session_address: Option<&str>,
) -> BTreeMap<String, String> {
    let mut context = BTreeMap::new();
    for (key, tool) in [
        ("installed_dbus_broker_version", HostTool::DbusBroker),
        ("installed_dbus_daemon_version", HostTool::DbusDaemon),
    ] {
        let value =
            read_tool(runner, tool, &["--version"]).unwrap_or_else(|e| format!("unavailable: {e}"));
        context.insert(key.into(), short(value.trim()));
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let version = if desktop.to_ascii_lowercase().contains("kde") {
        read_tool(
            runner,
            HostTool::PlasmaShell,
            &["--platform", "offscreen", "--version"],
        )
    } else if desktop.to_ascii_lowercase().contains("gnome") {
        read_tool(runner, HostTool::GnomeShell, &["--version"])
    } else {
        Err("desktop version is not available for this session".into())
    };
    context.insert(
        "desktop_version".into(),
        short(&version.unwrap_or_else(|e| format!("unavailable: {e}"))),
    );
    for name in [crate::power_bus::MODERN.0, crate::power_bus::COMPAT.0] {
        let value = read_tool(
            runner,
            HostTool::Busctl,
            &[
                "--system",
                "--timeout=1",
                "--auto-start=no",
                "call",
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                "s",
                name,
            ],
        );
        context.insert(
            format!("bus_owner.{name}"),
            value.unwrap_or_else(|e| format!("unavailable: {e}")),
        );
    }
    for (key, args) in [
        (
            "polkit_backend_version",
            vec![
                "--system",
                "--timeout=1",
                "--auto-start=no",
                "get-property",
                "org.freedesktop.PolicyKit1",
                "/org/freedesktop/PolicyKit1/Authority",
                "org.freedesktop.PolicyKit1.Authority",
                "BackendVersion",
            ],
        ),
        (
            "bus_features",
            vec![
                "--system",
                "--timeout=1",
                "--auto-start=no",
                "get-property",
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "Features",
            ],
        ),
    ] {
        context.insert(
            key.into(),
            read_tool(runner, HostTool::Busctl, &args)
                .unwrap_or_else(|e| format!("unavailable: {e}")),
        );
    }
    for unit in ["awccd.service", "dbus.service"]
        .into_iter()
        .chain(crate::provider_install::UNITS.iter().copied())
    {
        context.insert(
            format!("unit.{unit}"),
            read_tool(
                runner,
                HostTool::Systemctl,
                &[
                    "show",
                    unit,
                    "--property=LoadState,ActiveState,SubState,UnitFileState,FragmentPath,MainPID",
                ],
            )
            .unwrap_or_else(|e| format!("unavailable: {e}")),
        );
    }
    if let Some(address) = session_address.filter(|address| address.starts_with("unix:")) {
        let address_arg = format!("--address={address}");
        for member in [
            "currentProfile",
            "configuredProfile",
            "profileChoices",
            "profileHolds",
        ] {
            let args = [
                address_arg.as_str(),
                "--timeout=1",
                "--auto-start=no",
                "call",
                "org.kde.Solid.PowerManagement",
                "/org/kde/Solid/PowerManagement/Actions/PowerProfile",
                "org.kde.Solid.PowerManagement.Actions.PowerProfile",
                member,
            ];
            context.insert(
                format!("desktop_power.{member}"),
                read_tool(runner, HostTool::Busctl, &args)
                    .unwrap_or_else(|e| format!("unavailable: {e}")),
            );
        }
    } else {
        context.insert(
            "desktop_power".into(),
            "unavailable: no local session bus address".into(),
        );
    }
    context
}

pub fn collect(simulated: bool) -> ServiceSupport {
    if simulated {
        return ServiceSupport {
            state: "simulation".into(),
            gaps: vec!["Host diagnostics are disabled in simulation".into()],
            ..Default::default()
        };
    }
    collect_with(
        &DaemonClient::system("awcc-support", env!("CARGO_PKG_VERSION")),
        &SystemRunner::default(),
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
    )
}

pub fn collect_with(
    client: &DaemonClient,
    runner: &dyn HostRunner,
    session_address: Option<&str>,
) -> ServiceSupport {
    let mut support = ServiceSupport {
        state: "unavailable".into(),
        ..Default::default()
    };
    let mut cursor = 0;
    let mut collected = Vec::new();
    let mut first: Option<DaemonDiagnostics> = None;
    let mut completed = false;
    for _ in 0..64 {
        match client.diagnostics(cursor) {
            Ok(page) => {
                if first
                    .as_ref()
                    .is_some_and(|first| first.session_id != page.session_id)
                {
                    support
                        .gaps
                        .push("Daemon restarted during export; event collection stopped".into());
                    break;
                }
                let more = page.has_more;
                let next = page.next_seq;
                collected.extend(
                    page.events
                        .iter()
                        .take(MAX_DAEMON_EVENTS.saturating_sub(collected.len()))
                        .cloned(),
                );
                if first.is_none() {
                    first = Some(page);
                }
                if !more {
                    completed = true;
                    break;
                }
                if collected.len() >= MAX_DAEMON_EVENTS || next <= cursor {
                    support.gaps.push(
                        "Daemon event export limit reached; older or additional events are omitted"
                            .into(),
                    );
                    break;
                }
                cursor = next;
            }
            Err(error) => {
                support
                    .gaps
                    .push(format!("Daemon diagnostics unavailable: {error}"));
                break;
            }
        }
    }
    if let Some(mut page) = first {
        if let Some(note) = &page.history_note {
            support.gaps.push(note.clone());
        }
        if page
            .oldest_seq
            .zip(page.events.first().map(|event| event.seq))
            .is_some_and(|(oldest, first)| first > oldest)
        {
            support
                .gaps
                .push("Older retained daemon events omitted by the export event limit".into());
        }
        if page.console.dropped > 0 || page.console.failures > 0 {
            support.gaps.push(format!(
                "Console delivery incomplete: {} drops and {} failures",
                page.console.dropped, page.console.failures
            ));
        }
        if !page.file_sink.enabled
            || page.file_sink.dropped_queue_full > 0
            || page.file_sink.dropped_write_failed > 0
            || page.file_sink.write_failures > 0
        {
            support.gaps.push(format!("Daemon file logging incomplete: enabled={}, queue drops={}, write drops={}, failures={}",page.file_sink.enabled,page.file_sink.dropped_queue_full,page.file_sink.dropped_write_failed,page.file_sink.write_failures));
        }
        if page
            .power
            .get("payload_truncated")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            support
                .gaps
                .push("Cached power summary exceeded diagnostic IPC size limit".into());
        }
        if page.evicted > 0 {
            support.gaps.push(format!(
                "{} daemon events were evicted before export",
                page.evicted
            ));
        }
        if page.cleared > 0 {
            support.gaps.push(format!(
                "{} daemon events were cleared before export",
                page.cleared
            ));
        }
        if collected
            .iter()
            .chain(&page.history)
            .any(|event| event.fields.contains_key("payload_truncated"))
        {
            support.gaps.push(
                "Oversized event payloads were clipped to the diagnostic IPC field limits".into(),
            );
        }
        if !completed {
            support
                .gaps
                .push("Daemon event scan ended at a bounded page limit or collection error".into());
        }
        page.has_more = !completed;
        page.next_seq = collected.last().map_or(page.next_seq, |event| event.seq);
        page.events = collected;
        support.daemon = Some(page);
        support.state = "available".into();
    }
    support.context = integration_context(runner, session_address);
    // A fixed service journal only: never all system traffic or a caller-selected unit/path.
    match read_tool(
        runner,
        HostTool::Journalctl,
        &[
            "--unit=awccd.service",
            "--no-pager",
            "--output=short-iso",
            "--lines=64",
            "--since=-24h",
        ],
    ) {
        Ok(journal) => {
            support.startup_journal = journal
                .lines()
                .take(64)
                .map(|line| line.chars().take(1024).collect())
                .collect();
            if journal.contains("[diagnostic warning:") {
                support.gaps.push(
                    "Service journal tool reported limited visibility or another warning".into(),
                );
            }
            if journal.contains("[evidence truncated]") {
                support
                    .gaps
                    .push("Service journal excerpt truncated to the export byte limit".into());
            }
            if support.startup_journal.is_empty() || journal.contains("-- No entries --") {
                support.gaps.push("No accessible service journal entries; earlier startup failures may be unavailable".into());
            }
        }
        Err(error) => support
            .gaps
            .push(format!("Service journal unavailable: {error}")),
    }
    support
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::installer::{HostError, HostOutput};
    use std::sync::Mutex;
    struct ReadOnly {
        calls: Mutex<Vec<HostCommand>>,
        journal_code: i32,
    }
    impl HostRunner for ReadOnly {
        fn run(
            &self,
            command: &HostCommand,
            _: Option<&mut dyn FnMut(&str)>,
        ) -> Result<HostOutput, HostError> {
            self.calls.lock().unwrap().push(command.clone());
            let args = command
                .args
                .iter()
                .map(|arg| arg.to_str().unwrap())
                .collect::<Vec<_>>();
            match command.tool {
                HostTool::Busctl => {
                    assert!(args.contains(&"--auto-start=no"));
                    assert!(!args.contains(&"set-property"));
                    assert!(!args.contains(&"Set"));
                    assert!(!args.contains(&"Refresh"));
                    if let Some(call) = args.iter().position(|arg| *arg == "call") {
                        assert!(matches!(
                            args[call + 4],
                            "GetNameOwner"
                                | "currentProfile"
                                | "configuredProfile"
                                | "profileChoices"
                                | "profileHolds"
                        ));
                    } else {
                        assert!(args.contains(&"get-property"));
                        assert!(matches!(
                            args.last().copied(),
                            Some("BackendVersion" | "Features")
                        ));
                    }
                }
                HostTool::Systemctl => assert_eq!(args[0], "show"),
                HostTool::Journalctl => assert!(args.contains(&"--unit=awccd.service")),
                HostTool::DbusBroker
                | HostTool::DbusDaemon
                | HostTool::PlasmaShell
                | HostTool::GnomeShell => assert!(args.contains(&"--version")),
                _ => panic!("unexpected mutating tool"),
            }
            Ok(if command.tool == HostTool::Journalctl {
                HostOutput {
                    code: Some(self.journal_code),
                    stdout: if self.journal_code == 0 {
                        "entry\n".repeat(5000)
                    } else {
                        String::new()
                    },
                    stderr: "permission or visibility warning".into(),
                }
            } else {
                HostOutput {
                    code: Some(0),
                    stdout: "passive context".into(),
                    stderr: String::new(),
                }
            })
        }
    }
    #[test]
    fn unavailable_daemon_and_journal_are_explicit_and_context_cannot_activate_services() {
        let fake = ReadOnly {
            calls: Mutex::new(Vec::new()),
            journal_code: 1,
        };
        let client = DaemonClient::new("/nonexistent-awcc-diagnostic-test.sock", "test", "test");
        let support = collect_with(&client, &fake, Some("unix:path=/nonexistent-session-bus"));
        assert_eq!(support.state, "unavailable");
        assert!(support.daemon.is_none());
        assert!(
            support
                .gaps
                .iter()
                .any(|gap| gap.contains("Daemon diagnostics unavailable"))
        );
        assert!(
            support
                .gaps
                .iter()
                .any(|gap| gap.contains("Service journal unavailable"))
        );
        assert!(fake.calls.lock().unwrap().len() >= 10);
        assert_eq!(collect(true).state, "simulation");
    }
    #[test]
    fn limited_journal_visibility_and_truncation_are_reported() {
        let fake = ReadOnly {
            calls: Mutex::new(Vec::new()),
            journal_code: 0,
        };
        let client = DaemonClient::new("/nonexistent-awcc-diagnostic-test.sock", "test", "test");
        let support = collect_with(&client, &fake, None);
        assert!(support.startup_journal.len() <= 64);
        assert!(support.gaps.iter().any(|gap| gap.contains("truncated")));
        assert!(
            support
                .gaps
                .iter()
                .any(|gap| gap.contains("limited visibility"))
        );
    }
}
