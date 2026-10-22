//! Allowlist-oriented redaction for text and JSON that leaves the machine in a support bundle.
//!
//! The redactor never *collects* anything: it is told what the local user name, home directory,
//! and host name are (from the environment) only so it can remove them. Everything is done with
//! plain scanning (no regex dependency) and is idempotent.
//!
//! What is redacted:
//! - the home directory and any `/home/<name>`, `/var/home/<name>`, `/root`, `/run/user/<uid>`,
//!   `/run/media/<name>`, `/media/<name>` and AppImage `/tmp/.mount_*` path segment
//! - the local user name and host name (whole words, minimum 3 characters)
//! - UUIDs, MAC addresses, e-mail addresses
//! - values of `service tag`, `serial`, `uuid`, `asset tag`, `hostname`, `username`, `password`,
//!   `token`, `secret` keys, both in free text (`key=value`, `key: value`, JSON `"key":"value"`)
//!   and as JSON object keys
//!
//! What is deliberately preserved: model/profile identifiers (for example `Alienware m15 R6`,
//! `AMWW`, `0x0A86`), versions, error codes, and non-identifying numeric data.

use serde_json::Value;

pub const REDACTED: &str = "<redacted>";

/// Identifying strings of the current machine/user, used only to scrub them out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redactor {
    username: Option<String>,
    home: Option<String>,
    hostname: Option<String>,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Replace whole-word occurrences of `word` (case-sensitive) with `replacement`.
fn replace_word(text: &str, word: &str, replacement: &str) -> String {
    if word.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(word) {
        let before = rest[..pos].chars().next_back();
        let after = rest[pos + word.len()..].chars().next();
        let boundary_ok =
            before.is_none_or(|c| !is_word_char(c)) && after.is_none_or(|c| !is_word_char(c));
        out.push_str(&rest[..pos]);
        if boundary_ok {
            out.push_str(replacement);
        } else {
            out.push_str(word);
        }
        rest = &rest[pos + word.len()..];
    }
    out.push_str(rest);
    out
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn looks_like_uuid(s: &[u8]) -> bool {
    s.len() == 36
        && s.iter().enumerate().all(|(i, &b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                is_hex(b)
            }
        })
}

fn looks_like_mac(s: &[u8]) -> bool {
    s.len() == 17
        && s.iter().enumerate().all(|(i, &b)| {
            if i % 3 == 2 {
                b == b':' || b == b'-'
            } else {
                is_hex(b)
            }
        })
}

/// Replace UUID and MAC-address shaped tokens.
fn redact_tokens(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let prev_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if prev_ok && i + 36 <= bytes.len() && looks_like_uuid(&bytes[i..i + 36]) {
            let next_ok = i + 36 == bytes.len() || !bytes[i + 36].is_ascii_alphanumeric();
            if next_ok {
                out.push_str("<uuid>");
                i += 36;
                continue;
            }
        }
        if prev_ok && i + 17 <= bytes.len() && looks_like_mac(&bytes[i..i + 17]) {
            let next_ok = i + 17 == bytes.len() || !bytes[i + 17].is_ascii_alphanumeric();
            if next_ok {
                out.push_str("<mac>");
                i += 17;
                continue;
            }
        }
        // Copy one full UTF-8 character.
        let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Replace `local@domain.tld` shaped tokens.
fn redact_emails(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (idx, token) in text.split_inclusive(char::is_whitespace).enumerate() {
        let _ = idx;
        let trimmed = token.trim_end_matches(char::is_whitespace);
        let trailing = &token[trimmed.len()..];
        if let Some(at) = trimmed.find('@')
            && at > 0
            && trimmed[at + 1..].contains('.')
            && !trimmed.contains('/')
        {
            out.push_str("<email>");
        } else {
            out.push_str(trimmed);
        }
        out.push_str(trailing);
    }
    out
}

const SENSITIVE_KEYS: &[&str] = &[
    "servicetag",
    "serialnumber",
    "serialno",
    "serial",
    "productserial",
    "chassisserial",
    "boardserial",
    "uuid",
    "assettag",
    "hostname",
    "username",
    "password",
    "passwd",
    "token",
    "secret",
];

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// `true` when a JSON object key names an identifying/secret value.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    SENSITIVE_KEYS.iter().any(|k| normalized.contains(k))
}

/// Redact `key=value` / `key: value` / `"key": "value"` where `key` is sensitive.
fn redact_key_values(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Candidate key start: a word boundary.
        let at_boundary = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric());
        if at_boundary && bytes[i].is_ascii_alphabetic() {
            // Take a key token: letters, digits, space-separated words, `_`, `-`.
            let mut end = i;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'-'))
            {
                end += 1;
            }
            // Allow a two-word key ("service tag", "serial number", "asset tag").
            let mut key_end = end;
            if key_end < bytes.len() && bytes[key_end] == b' ' {
                let mut next = key_end + 1;
                while next < bytes.len() && bytes[next].is_ascii_alphabetic() {
                    next += 1;
                }
                let two_word = &lower[i..next];
                if is_sensitive_key(two_word) && !is_sensitive_key(&lower[i..end]) {
                    key_end = next;
                }
            }
            let key = &lower[i..key_end];
            if is_sensitive_key(key) {
                let mut j = key_end;
                if j < bytes.len() && bytes[j] == b'"' {
                    j += 1;
                }
                while j < bytes.len() && bytes[j] == b' ' {
                    j += 1;
                }
                if j < bytes.len() && (bytes[j] == b':' || bytes[j] == b'=') {
                    j += 1;
                    while j < bytes.len() && bytes[j] == b' ' {
                        j += 1;
                    }
                    let quoted = j < bytes.len() && bytes[j] == b'"';
                    let value_start = if quoted { j + 1 } else { j };
                    let mut value_end = value_start;
                    while value_end < bytes.len() {
                        let b = bytes[value_end];
                        if quoted {
                            if b == b'"' {
                                break;
                            }
                        } else if b.is_ascii_whitespace() || matches!(b, b',' | b';' | b'}' | b')')
                        {
                            break;
                        }
                        value_end += 1;
                    }
                    if value_end > value_start {
                        out.push_str(&text[cursor..value_start]);
                        out.push_str(REDACTED);
                        cursor = value_end;
                        i = value_end;
                        continue;
                    }
                }
            }
            i = end.max(i + 1);
            continue;
        }
        i += 1;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Replace private path segments independent of the current user.
fn redact_private_paths(text: &str) -> String {
    let mut out = text.to_string();
    for (prefix, replacement) in [
        ("/var/home/", "~/"),
        ("/home/", "~/"),
        ("/run/media/", "/run/media/<user>/"),
        ("/media/", "/media/<user>/"),
        ("/run/user/", "/run/user/<uid>/"),
        ("/tmp/.mount_", "/tmp/.mount_<id>/"),
    ] {
        out = replace_path_segment(&out, prefix, replacement);
    }
    // The root account's home.
    out = replace_word_path(&out, "/root", "~root");
    out
}

/// Replace `<prefix><segment>` (segment up to the next `/` or end of token) with `replacement`
/// (which already ends the replaced segment, so the following `/rest` is kept).
fn replace_path_segment(text: &str, prefix: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(prefix) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + prefix.len()..];
        let seg_len = after
            .find(|c: char| {
                c == '/' || c.is_whitespace() || matches!(c, '"' | '\'' | ':' | ',' | ')')
            })
            .unwrap_or(after.len());
        if seg_len == 0 || after[..seg_len].starts_with('<') || after[..seg_len] == *"~" {
            out.push_str(prefix);
            rest = after;
            continue;
        }
        // Keep an already-redacted segment untouched (idempotence).
        out.push_str(replacement.trim_end_matches('/'));
        rest = &after[seg_len..];
    }
    out.push_str(rest);
    out
}

fn replace_word_path(text: &str, path: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(path) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + path.len()..];
        let boundary = after.chars().next().is_none_or(|c| !is_word_char(c));
        out.push_str(if boundary { replacement } else { path });
        rest = after;
    }
    out.push_str(rest);
    out
}

impl Redactor {
    /// A redactor that knows nothing machine-specific (still scrubs patterns and keys).
    #[must_use]
    pub fn generic() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn new(username: Option<String>, home: Option<String>, hostname: Option<String>) -> Self {
        Self {
            username: username.filter(|u| u.len() >= 3),
            home: home.filter(|h| h.len() > 1),
            hostname: hostname.filter(|h| h.len() >= 3 && !h.starts_with("localhost")),
        }
    }

    /// Learn the local identifiers from the environment and `/proc`, only to remove them later.
    #[must_use]
    pub fn from_environment() -> Self {
        let username = std::env::var("USER")
            .ok()
            .or_else(|| std::env::var("LOGNAME").ok())
            .filter(|s| !s.is_empty());
        let home = std::env::var("HOME").ok().filter(|s| s.starts_with('/'));
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self::new(username, home, hostname)
    }

    /// Redact identifying values in free text (log lines, error chains, backtraces).
    #[must_use]
    pub fn redact_text(&self, text: &str) -> String {
        let mut out = text.to_string();
        if let Some(home) = &self.home {
            out = out.replace(home.as_str(), "~");
        }
        out = redact_private_paths(&out);
        if let Some(user) = &self.username {
            out = replace_word(&out, user, "<user>");
        }
        if let Some(host) = &self.hostname {
            out = replace_word(&out, host, "<hostname>");
        }
        out = redact_tokens(&out);
        out = redact_key_values(&out);
        redact_emails(&out)
    }

    /// Redact every string in a JSON value and blank values under sensitive keys.
    pub fn redact_json(&self, value: &mut Value) {
        match value {
            Value::String(s) => *s = self.redact_text(s),
            Value::Array(items) => items.iter_mut().for_each(|v| self.redact_json(v)),
            Value::Object(map) => {
                for (key, v) in map.iter_mut() {
                    if is_sensitive_key(key) && !v.is_null() {
                        *v = Value::String(REDACTED.to_string());
                    } else {
                        self.redact_json(v);
                    }
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn red() -> Redactor {
        Redactor::new(
            Some("alice".to_string()),
            Some("/home/alice".to_string()),
            Some("alienbox".to_string()),
        )
    }

    #[test]
    fn private_paths_in_error_chains_and_backtraces_are_redacted() {
        let r = red();
        let text = "failed to open /home/alice/.local/state/awcc-rs/logs/session-1.jsonl: Permission denied (os error 13)\n\
                    stack backtrace:\n   0: awcc::main\n             at /home/alice/Projects/awcc/src/main.rs:42:5\n\
                    caused by: /run/user/1000/wayland-0: not found\n at /home/bob/.cargo/registry/src/x/lib.rs:1";
        let out = r.redact_text(text);
        assert!(!out.contains("alice"), "{out}");
        assert!(!out.contains("bob"), "{out}");
        assert!(!out.contains("/run/user/1000"), "{out}");
        assert!(
            out.contains("~/.local/state/awcc-rs/logs/session-1.jsonl"),
            "{out}"
        );
        assert!(out.contains("~/Projects/awcc/src/main.rs:42:5"), "{out}");
        assert!(out.contains("/run/user/<uid>/wayland-0"), "{out}");
        assert!(out.contains("os error 13"), "error context is kept: {out}");
    }

    #[test]
    fn user_and_host_names_are_removed_as_whole_words_only() {
        let r = red();
        assert_eq!(
            r.redact_text("user alice on alienbox"),
            "user <user> on <hostname>"
        );
        // Not a whole word: untouched.
        assert_eq!(
            r.redact_text("alicedoe palice alienboxes"),
            "alicedoe palice alienboxes"
        );
        // Short names are never replaced as words (too many false positives).
        let short = Redactor::new(Some("al".into()), None, Some("pc".into()));
        assert_eq!(short.redact_text("al pc"), "al pc");
    }

    #[test]
    fn identifying_values_are_redacted_in_free_text() {
        let r = Redactor::generic();
        let text = "product_serial=ABC1234 service tag: 7XYZ9K1 uuid 123e4567-e89b-12d3-a456-426614174000 \
                    mac aa:bb:cc:dd:ee:ff mail bob@example.com {\"serial\": \"S/N 99\", \"hostname\":\"box\"}";
        let out = r.redact_text(text);
        for secret in [
            "ABC1234",
            "7XYZ9K1",
            "123e4567",
            "aa:bb:cc",
            "bob@example",
            "S/N 99",
            "\"box\"",
        ] {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        assert!(
            out.contains("<uuid>") && out.contains("<mac>") && out.contains("<email>"),
            "{out}"
        );
    }

    #[test]
    fn model_and_profile_identifiers_survive() {
        let r = red();
        let text = "Alienware m15 R6 · AMWW (\\_SB.AMWW.WMAX) · SMBIOS 0x0A86 · 0.1.0 · rustc 1.95.0 · error code turbo_write_rejected";
        assert_eq!(r.redact_text(text), text);
    }

    #[test]
    fn json_values_and_sensitive_keys_are_redacted_recursively() {
        let r = red();
        let mut v = json!({
            "model": "Alienware m15 R6",
            "service_tag": "ABC1234",
            "dmi": {"product_serial": "XYZ", "product_uuid": "123e4567-e89b-12d3-a456-426614174000", "sku": "0A86"},
            "errors": ["cannot read /home/alice/db.json", "host alienbox down"],
            "n": 5
        });
        r.redact_json(&mut v);
        let s = v.to_string();
        for secret in ["ABC1234", "XYZ", "123e4567", "alice", "alienbox"] {
            assert!(!s.contains(secret), "{secret} leaked: {s}");
        }
        assert_eq!(v["model"], "Alienware m15 R6");
        assert_eq!(v["dmi"]["sku"], "0A86");
        assert_eq!(v["service_tag"], REDACTED);
        assert_eq!(v["n"], 5);
    }

    #[test]
    fn redaction_is_idempotent() {
        let r = red();
        let once = r.redact_text("open /home/alice/x as alice on alienbox serial=ZZ9");
        assert_eq!(r.redact_text(&once), once);
    }

    #[test]
    fn appimage_mount_and_removable_media_paths_are_scrubbed() {
        let r = Redactor::generic();
        let out =
            r.redact_text("/tmp/.mount_awccAb12Cd/usr/bin/awcc-cli and /run/media/carol/USB/f");
        assert!(
            !out.contains("awccAb12Cd") && !out.contains("carol"),
            "{out}"
        );
        assert!(out.contains("/usr/bin/awcc-cli"), "{out}");
    }
}
