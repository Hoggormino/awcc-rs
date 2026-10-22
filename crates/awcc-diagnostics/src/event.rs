//! Structured event model shared by the CLI, the GUI, and support exports.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Event severity (ordered: `Error` is the most severe, `Trace` the most verbose).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Severity {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }

    #[must_use]
    pub const fn from_tracing(level: tracing::Level) -> Self {
        match level {
            tracing::Level::ERROR => Self::Error,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::INFO => Self::Info,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::TRACE => Self::Trace,
        }
    }
}

/// Which stream an event belongs to. Hardware *mutation* records stay in the separate write-audit
/// stream; the `hardware_write` category only marks the application event that points at one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// General application event (startup, settings, operation results, worker exit, ...).
    #[default]
    App,
    /// A passive diagnostic/state read (never a hardware mutation).
    DiagnosticRead,
    /// Pointer to a mutating hardware write recorded in the write-audit stream (`audit_seq`).
    HardwareWrite,
}

impl Category {
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "diagnostic_read" => Self::DiagnosticRead,
            "hardware_write" => Self::HardwareWrite,
            _ => Self::App,
        }
    }
}

/// One recorded event. `seq` is assigned at emission, so it identifies the event for its whole
/// life (ring, snapshots, presentation models, files, exports) — causality is never rebuilt by
/// parsing message text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub session_id: String,
    /// Per-session emission sequence (`1`-based; only recorded events consume numbers).
    pub seq: u64,
    pub ts_utc: String,
    pub unix_ms: u64,
    pub severity: Severity,
    pub component: String,
    #[serde(default)]
    pub category: Category,
    /// Operation this event belongs to (carried across queue, service, transport, rollback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_id: Option<u64>,
    /// Requested action (for example `SetCpuTurbo(false)` label), never parsed back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Stable machine-readable error code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Monotonic elapsed time of the operation/attempt, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// Number of identical events folded into this one by rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_count: Option<u64>,
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

impl crate::ring::HasSeq for Event {
    fn seq(&self) -> u64 {
        self.seq
    }
}

impl Event {
    /// One-line human rendering used for the console and the GUI list.
    #[must_use]
    pub fn render_line(&self) -> String {
        let mut line = format!(
            "{} {:<5} {}",
            self.ts_utc,
            self.severity.as_str(),
            self.component
        );
        if let Some(op) = self.op_id {
            line.push_str(&format!(" op={op}"));
        }
        if let Some(outcome) = &self.outcome {
            line.push_str(&format!(" outcome={outcome}"));
        }
        if let Some(code) = &self.code {
            line.push_str(&format!(" code={code}"));
        }
        if let Some(ms) = self.elapsed_ms {
            line.push_str(&format!(" elapsed={ms}ms"));
        }
        if let Some(n) = self.repeat_count {
            line.push_str(&format!(" repeated={n}x"));
        }
        line.push_str(": ");
        line.push_str(&self.message);
        for (k, v) in &self.fields {
            line.push_str(&format!(" {k}={v}"));
        }
        line
    }
}
