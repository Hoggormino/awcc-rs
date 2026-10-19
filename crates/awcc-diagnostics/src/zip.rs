//! Minimal ZIP writer (method 0 / "stored") plus a matching reader for verification.
//!
//! Support bundles are small and bounded, so compression is unnecessary; a stored archive keeps
//! the implementation tiny, auditable, and free of extra dependencies. Entry names are restricted
//! to a safe relative ASCII subset so a bundle can never carry a traversal path.

use thiserror::Error;

use crate::hash::crc32;
use crate::time::dos_datetime;

/// One file to place in the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ZipError {
    #[error("invalid archive entry name `{0}`")]
    InvalidName(String),
    #[error("archive entry `{0}` is too large")]
    TooLarge(String),
    #[error("too many archive entries")]
    TooManyEntries,
    #[error("malformed archive: {0}")]
    Malformed(&'static str),
}

const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;

fn valid_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('/')
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Serialise `entries` as a stored ZIP archive stamped with `modified_ms` (Unix milliseconds).
pub fn write_zip(entries: &[ZipEntry], modified_ms: u64) -> Result<Vec<u8>, ZipError> {
    if entries.len() > usize::from(u16::MAX) {
        return Err(ZipError::TooManyEntries);
    }
    let (dos_date, dos_time) = dos_datetime(modified_ms);
    let mut out = Vec::new();
    let mut central = Vec::new();

    for entry in entries {
        if !valid_entry_name(&entry.name) {
            return Err(ZipError::InvalidName(entry.name.clone()));
        }
        let size =
            u32::try_from(entry.data.len()).map_err(|_| ZipError::TooLarge(entry.name.clone()))?;
        let name_len = u16::try_from(entry.name.len())
            .map_err(|_| ZipError::InvalidName(entry.name.clone()))?;
        let crc = crc32(&entry.data);
        let offset =
            u32::try_from(out.len()).map_err(|_| ZipError::TooLarge(entry.name.clone()))?;

        push_u32(&mut out, LOCAL_SIG);
        push_u16(&mut out, 20); // version needed
        push_u16(&mut out, 0); // flags
        push_u16(&mut out, 0); // method: stored
        push_u16(&mut out, dos_time);
        push_u16(&mut out, dos_date);
        push_u32(&mut out, crc);
        push_u32(&mut out, size);
        push_u32(&mut out, size);
        push_u16(&mut out, name_len);
        push_u16(&mut out, 0); // extra length
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(&entry.data);

        push_u32(&mut central, CENTRAL_SIG);
        push_u16(&mut central, 20); // version made by
        push_u16(&mut central, 20); // version needed
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, dos_time);
        push_u16(&mut central, dos_date);
        push_u32(&mut central, crc);
        push_u32(&mut central, size);
        push_u32(&mut central, size);
        push_u16(&mut central, name_len);
        push_u16(&mut central, 0); // extra
        push_u16(&mut central, 0); // comment
        push_u16(&mut central, 0); // disk start
        push_u16(&mut central, 0); // internal attrs
        push_u32(&mut central, 0o100_600 << 16); // external attrs: regular file, 0600
        push_u32(&mut central, offset);
        central.extend_from_slice(entry.name.as_bytes());
    }

    let central_offset =
        u32::try_from(out.len()).map_err(|_| ZipError::TooLarge("archive".to_string()))?;
    let central_len =
        u32::try_from(central.len()).map_err(|_| ZipError::TooLarge("archive".to_string()))?;
    out.extend_from_slice(&central);
    let count = u16::try_from(entries.len()).map_err(|_| ZipError::TooManyEntries)?;
    push_u32(&mut out, EOCD_SIG);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);
    push_u16(&mut out, count);
    push_u16(&mut out, count);
    push_u32(&mut out, central_len);
    push_u32(&mut out, central_offset);
    push_u16(&mut out, 0);
    Ok(out)
}

fn rd_u16(b: &[u8], at: usize) -> Result<u16, ZipError> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or(ZipError::Malformed("truncated"))
}

fn rd_u32(b: &[u8], at: usize) -> Result<u32, ZipError> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or(ZipError::Malformed("truncated"))
}

/// Read back an archive produced by [`write_zip`], verifying names, bounds, and CRCs.
pub fn read_stored_zip(bytes: &[u8]) -> Result<Vec<ZipEntry>, ZipError> {
    if bytes.len() < 22 {
        return Err(ZipError::Malformed("too short"));
    }
    let eocd = bytes.len() - 22;
    if rd_u32(bytes, eocd)? != EOCD_SIG {
        return Err(ZipError::Malformed("missing end of central directory"));
    }
    let count = usize::from(rd_u16(bytes, eocd + 10)?);
    let mut at =
        usize::try_from(rd_u32(bytes, eocd + 16)?).map_err(|_| ZipError::Malformed("offset"))?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if rd_u32(bytes, at)? != CENTRAL_SIG {
            return Err(ZipError::Malformed("bad central header"));
        }
        let method = rd_u16(bytes, at + 10)?;
        let crc = rd_u32(bytes, at + 16)?;
        let size =
            usize::try_from(rd_u32(bytes, at + 24)?).map_err(|_| ZipError::Malformed("size"))?;
        let name_len = usize::from(rd_u16(bytes, at + 28)?);
        let extra_len = usize::from(rd_u16(bytes, at + 30)?);
        let comment_len = usize::from(rd_u16(bytes, at + 32)?);
        let local =
            usize::try_from(rd_u32(bytes, at + 42)?).map_err(|_| ZipError::Malformed("offset"))?;
        let name = std::str::from_utf8(
            bytes
                .get(at + 46..at + 46 + name_len)
                .ok_or(ZipError::Malformed("name"))?,
        )
        .map_err(|_| ZipError::Malformed("name encoding"))?
        .to_string();
        if method != 0 {
            return Err(ZipError::Malformed("unsupported method"));
        }
        if !valid_entry_name(&name) {
            return Err(ZipError::InvalidName(name));
        }
        if rd_u32(bytes, local)? != LOCAL_SIG {
            return Err(ZipError::Malformed("bad local header"));
        }
        let local_name = usize::from(rd_u16(bytes, local + 26)?);
        let local_extra = usize::from(rd_u16(bytes, local + 28)?);
        let start = local + 30 + local_name + local_extra;
        let data = bytes
            .get(start..start + size)
            .ok_or(ZipError::Malformed("data"))?
            .to_vec();
        if crc32(&data) != crc {
            return Err(ZipError::Malformed("crc mismatch"));
        }
        entries.push(ZipEntry { name, data });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(name: &str, data: &[u8]) -> ZipEntry {
        ZipEntry {
            name: name.to_string(),
            data: data.to_vec(),
        }
    }

    #[test]
    fn roundtrips_entries_and_verifies_crc() {
        let entries = vec![
            entry("summary.txt", b"hello"),
            entry("logs/a.jsonl", b"{}\n"),
        ];
        let bytes = write_zip(&entries, 1_760_000_000_000).unwrap();
        assert_eq!(read_stored_zip(&bytes).unwrap(), entries);

        let mut corrupted = bytes.clone();
        let pos = corrupted
            .windows(5)
            .position(|w| w == b"hello")
            .expect("payload present");
        corrupted[pos] ^= 0xFF;
        assert_eq!(
            read_stored_zip(&corrupted),
            Err(ZipError::Malformed("crc mismatch"))
        );
    }

    #[test]
    fn traversal_and_odd_names_are_rejected() {
        for bad in [
            "../x",
            "/abs",
            "a//b",
            "a/../b",
            "",
            "sp ace.txt",
            "é.txt",
            "a\\b",
        ] {
            assert_eq!(
                write_zip(&[entry(bad, b"x")], 0),
                Err(ZipError::InvalidName(bad.to_string())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn empty_archive_is_valid() {
        let bytes = write_zip(&[], 0).unwrap();
        assert_eq!(bytes.len(), 22);
        assert!(read_stored_zip(&bytes).unwrap().is_empty());
    }
}
