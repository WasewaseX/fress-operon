//! Host-side download plumbing — faithful ports of the original helper
//! functions from Fress's Rust core (staging files, validator sidecars,
//! numbered names, Content-Range validation). The BRAIN (op/fress.op) owns
//! the decisions; this module is pure filesystem/HTTP mechanics.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let mut candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    for i in 1..10_000u32 {
        candidate = dir.join(numbered_name(name, i));
        if !candidate.exists() {
            break;
        }
    }
    candidate
}

/// "setup.exe", 3 -> "setup (3).exe"; extension-less names keep no dot.
pub fn numbered_name(name: &str, i: u32) -> String {
    if i == 0 {
        return name.to_string();
    }
    let p = PathBuf::from(name);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".into());
    let ext = p
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if ext.is_empty() {
        format!("{} ({})", stem, i)
    } else {
        format!("{} ({}).{}", stem, i, ext)
    }
}

/// Staging file for an in-flight download. The final name only appears when
/// the download finished (and verified, when a trusted hash was supplied),
/// so a dropped connection can never leave a half-written "installer.exe"
/// behind that a retry would then call "installer (1).exe".
pub fn part_path_for(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|s| format!("{}.part", s.to_string_lossy()))
        .unwrap_or_else(|| "download.part".to_string());
    match dest.parent() {
        Some(p) => p.join(name),
        None => PathBuf::from(name),
    }
}

/// Inverse of part_path_for: the final destination a staging file belongs
/// to. "dir/setup.exe.part" -> "dir/setup.exe"; anything not ending in
/// ".part" has no destination.
pub fn dest_for_part(part: &Path) -> Option<PathBuf> {
    let name = part.file_name()?.to_string_lossy();
    let stem = name.strip_suffix(".part")?;
    part.parent().map(|p| p.join(stem))
}

/// Sidecar for a staging file: the HTTP validators (ETag / Last-Modified)
/// the origin served the staged bytes with.
pub fn meta_path_for(part: &Path) -> PathBuf {
    PathBuf::from(format!("{}.meta", part.to_string_lossy()))
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PartMeta {
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub fn read_part_meta(part: &Path) -> Option<PartMeta> {
    let raw = std::fs::read_to_string(meta_path_for(part)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist the validators from `headers` for the staging file `part`.
/// Failures are deliberately silent: a missing sidecar only means the next
/// resume cannot send If-Range and behaves like before.
pub fn write_part_meta(part: &Path, url: &str, etag: Option<&str>, last_modified: Option<&str>) {
    let meta = PartMeta {
        url: url.to_string(),
        etag: etag.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
        last_modified: last_modified
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
    };
    if let Ok(json) = serde_json::to_string(&meta) {
        let _ = std::fs::write(meta_path_for(part), json);
    }
}

/// Validate the `Content-Range` header of a single-range `206 Partial
/// Content` answer against the offset we asked to resume from.
///
/// RFC 9110 section 15.3.7: a 206 for a single range MUST carry a
/// `Content-Range: bytes N-M/T` (or `bytes N-M/*` when the total is
/// unknown). The rules are strict:
///   - the unit MUST be `bytes` (case-insensitive),
///   - the start N MUST equal `expected_start`,
///   - the end M MUST parse and be >= N,
///   - the total T, when present (not `*`), MUST be greater than M.
///
/// On success returns the number of bytes the range promised
/// (`end - start + 1`) so the caller can verify the body actually
/// delivered that many bytes before declaring the download complete.
pub fn validate_resume_content_range(header: &str, expected_start: u64) -> Result<u64, String> {
    let mut parts = header.split_whitespace();
    let unit = parts
        .next()
        .ok_or_else(|| "Content-Range header is empty".to_string())?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return Err(format!("unsupported Content-Range unit \"{}\"", unit));
    }
    let range = parts
        .next()
        .ok_or_else(|| "Content-Range carries no byte range".to_string())?;
    let (range_part, total_part) = range
        .split_once('/')
        .ok_or_else(|| "Content-Range has no \"/\" separator".to_string())?;
    let (start_s, end_s) = range_part
        .split_once('-')
        .ok_or_else(|| "Content-Range range has no \"-\" separator".to_string())?;
    let start: u64 = start_s
        .parse()
        .map_err(|_| format!("Content-Range start \"{}\" is not a number", start_s))?;
    let end: u64 = end_s
        .parse()
        .map_err(|_| format!("Content-Range end \"{}\" is not a number", end_s))?;
    if start != expected_start {
        return Err(format!(
            "Server resumed at offset {} but the staging file ends at {}",
            start, expected_start
        ));
    }
    if end < start {
        return Err(format!(
            "Content-Range end {} precedes its start {}",
            end, start
        ));
    }
    if total_part != "*" {
        let total: u64 = total_part
            .parse()
            .map_err(|_| format!("Content-Range total \"{}\" is not a number", total_part))?;
        if total <= end {
            return Err(format!(
                "Content-Range total {} does not exceed its end {}",
                total, end
            ));
        }
    }
    Ok(end - start + 1)
}
