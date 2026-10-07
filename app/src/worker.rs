//! The streaming download worker — the BODY half of the download engine.
//! Faithful port of the original run_download semantics (sync Rust on a
//! worker thread instead of tokio):
//!
//!   - resume with HTTP validators (ETag / Last-Modified sidecar,
//!     If-Range, and the byte-identity rule: validator-less staging bytes
//!     are discarded unless a published checksum will verify them)
//!   - 416 on a resume -> staging discarded, "resume-invalid" error
//!   - strict single-range Content-Range validation on a 206
//!   - 60 s stall -> transient "network" error (the brain's retry policy
//!     auto-resumes from the .part)
//!   - EOF is not completion: a truncated body is refused, the .part
//!     survives for Resume
//!   - progress at most ~10x per second with a truthful 4 s rolling speed
//!   - deliberate cancel keeps the .part
//!
//! The DECISIONS stay in the brain: retry policy, checksum verdicts,
//! filename sanitization (the worker extracts raw candidates from headers
//! and asks the brain through a sanitize round trip).

use crate::util::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub id: u32,
    pub url: String,
    pub dir: String,
    pub name: Option<String>,
    pub resume: bool,
    pub expected_sha256: String,
    /// The staging file a retry continues (carried across attempts so the
    /// retry continues THAT exact file instead of re-deriving a candidate
    /// which can drift onto a concurrent download's .part).
    #[serde(default)]
    pub staging: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub id: u32,
    pub outcome: String, // "complete" | "error" | "cancelled"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staging: Option<String>,
    /// The (sanitized) final candidate name for the completion rename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Ask the event loop to run the brain's sanitize gene for a raw
/// header/URL-derived candidate name (the policy lives in the brain).
pub enum WorkerMsg {
    Sanitize { raw: String, reply: SyncSender<String> },
}

struct Fail {
    message: String,
    kind: Option<String>,
    staging: Option<PathBuf>,
    cancelled: bool,
}

impl Fail {
    fn plain(message: String) -> Fail {
        Fail { message, kind: None, staging: None, cancelled: false }
    }
    fn typed(message: String, kind: &str) -> Fail {
        Fail { message, kind: Some(kind.to_string()), staging: None, cancelled: false }
    }
    fn cancelled() -> Fail {
        Fail { message: "__cancelled__".into(), kind: None, staging: None, cancelled: true }
    }
    fn with_staging(mut self, p: PathBuf) -> Fail {
        if self.staging.is_none() {
            self.staging = Some(p);
        }
        self
    }
}

fn agent() -> Result<ureq::Agent, String> {
    Ok(ureq::AgentBuilder::new()
        .user_agent(super::net::USER_AGENT)
        .timeout_connect(Duration::from_secs(15))
        // No overall request timeout: a real download runs for minutes.
        // The 60 s read timeout below is the stall detector.
        .timeout_read(Duration::from_secs(60))
        .redirects(5)
        .build())
}

/// Extract a raw filename candidate from Content-Disposition (RFC 5987
/// ext-value first) or the final URL's last path segment. Every candidate
/// still passes through the brain's sanitize before it touches disk.
fn raw_name_candidate(url: &str, cd: Option<&str>) -> Option<String> {
    if let Some(cd) = cd {
        if let Some(pos) = cd.find("filename*=") {
            let rest = &cd[pos + 10..];
            if let Some(name) = rest.split("''").nth(1) {
                let name = name.trim_matches('"').trim_end_matches(';');
                return Some(percent_decode(name));
            }
        }
        if let Some(pos) = cd.find("filename=") {
            let rest = &cd[pos + 9..];
            let end = rest.find(';').unwrap_or(rest.len());
            return Some(percent_decode(rest[..end].trim_matches('"')));
        }
    }
    // Fall back to the last URL path segment (no query). Segments come
    // percent-encoded; decode for a readable name. A segment without a
    // dot is rejected (no usable file name there).
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let segment = path.rsplit('/').next().unwrap_or("");
    if segment.is_empty() {
        return None;
    }
    let decoded = percent_decode(segment);
    if !decoded.contains('.') {
        return None;
    }
    Some(decoded)
}

/// Round trip through the brain's sanitize gene. Falls back to the safe
/// default if the host bridge is somehow gone.
fn sanitize_via_brain(raw: String, proxy: &tao::event_loop::EventLoopProxy<super::Msg>) -> String {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let _ = proxy.send_event(super::Msg::Sanitize { raw: raw.clone(), reply: tx });
    rx.recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| "download.bin".to_string())
}

pub fn run_download(
    plan: Plan,
    cancel: Arc<AtomicBool>,
    proxy: tao::event_loop::EventLoopProxy<super::Msg>,
) -> Report {
    match run_inner(&plan, &cancel, &proxy) {
        Ok(mut r) => {
            r.id = plan.id;
            r
        }
        Err(f) => Report {
            id: plan.id,
            outcome: if f.cancelled { "cancelled".into() } else { "error".into() },
            staging: f.staging.map(|p| p.to_string_lossy().into()),
            name: None,
            bytes: None,
            sha256: None,
            message: if f.cancelled { None } else { Some(f.message) },
            kind: f.kind,
        },
    }
}

fn run_inner(
    plan: &Plan,
    cancel: &Arc<AtomicBool>,
    proxy: &tao::event_loop::EventLoopProxy<super::Msg>,
) -> Result<Report, Fail> {
    let agent = agent().map_err(Fail::plain)?;
    let dir = PathBuf::from(&plan.dir);

    // --- resume probe -------------------------------------------------
    let mut planned_resume_len: u64 = 0;
    let mut resume_paths: Option<(PathBuf, PathBuf)> = None; // (dest, part)
    let mut resume_validator: Option<String> = None;
    let checksum_will_verify = !plan.expected_sha256.trim().is_empty();
    if plan.resume {
        let probe: Option<PathBuf> = match plan.staging.as_ref() {
            Some(p) => Some(PathBuf::from(p)),
            None => plan.name.as_ref().map(|n| part_path_for(&dir.join(n))),
        };
        if let Some(p) = probe {
            if let Ok(meta) = std::fs::metadata(&p) {
                if let Some(dest) = dest_for_part(&p) {
                    planned_resume_len = meta.len();
                    resume_paths = Some((dest, p.clone()));
                }
            }
            // The validator is only meaningful for the same resource the
            // staging bytes came from.
            if let Some(m) = read_part_meta(&p) {
                if m.url == plan.url {
                    resume_validator = m.etag.or(m.last_modified);
                }
            }
            // Byte-identity rule: staging bytes may only be resumed when
            // their origin can still be PROVEN. Without a validator, a
            // Range request would glue an old prefix onto a possibly
            // changed remote suffix; unless a published checksum will
            // verify the assembled file anyway, discard.
            if planned_resume_len > 0 && resume_validator.is_none() && !checksum_will_verify {
                let _ = std::fs::remove_file(&p);
                let _ = std::fs::remove_file(meta_path_for(&p));
                planned_resume_len = 0;
                resume_paths = None;
            }
            // A missing staging file simply falls through to a fresh
            // download, exactly like a resume without a .part.
        }
    }

    // --- request -------------------------------------------------------
    let mut req = agent.get(&plan.url);
    if planned_resume_len > 0 {
        req = req.header("Range", format!("bytes={}-", planned_resume_len));
        // If-Range makes a changed remote file answer with a plain 200
        // (full body) instead of a 206 tail - the 200 path restarts in
        // place instead of gluing an old prefix to a new suffix.
        if let Some(v) = &resume_validator {
            req = req.header("If-Range", v.clone());
        }
    }
    let first = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_code, r)) => r,
        Err(e) => return Err(Fail::plain(format!("Connection failed: {}", e))),
    };
    let status = first.status();
    let final_url = first.get_url().to_string();
    let etag = first.header("etag").map(|s| s.to_string());
    let last_modified = first.header("last-modified").map(|s| s.to_string());
    let content_disposition = first.header("content-disposition").map(|s| s.to_string());
    let content_range = first.header("content-range").map(|s| s.to_string());
    let remote_len = first
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    if !(200..300).contains(&status) {
        // 416 on a resume attempt: the byte range we hold can no longer be
        // satisfied. The staging bytes are worthless - discard and report
        // resume-invalid (the UI offers Retry, never Resume).
        if status == 416 && planned_resume_len > 0 {
            if let Some((_, p)) = resume_paths.as_ref() {
                let _ = std::fs::remove_file(p);
                let _ = std::fs::remove_file(meta_path_for(p));
            }
            return Err(Fail::typed(
                "Server reports the saved byte range is no longer valid (HTTP 416); the partial download was discarded".into(),
                "resume-invalid",
            ));
        }
        return Err(Fail::plain(format!("Server returned HTTP {}", status)));
    }

    // 206 = the server honored the Range and we append; a plain 200 means
    // it ignored the range (or If-Range no longer matches) and we start
    // over. A single-range 206 MUST carry a verifiable Content-Range.
    let resuming = status == 206 && planned_resume_len > 0;
    let already_have = if resuming { planned_resume_len } else { 0 };
    let expected_range_len: Option<u64> = if resuming {
        let verdict = match content_range.as_deref() {
            Some(h) => validate_resume_content_range(h, planned_resume_len),
            None => Err("Server answered a partial body without Content-Range; the byte offset cannot be verified".to_string()),
        };
        match verdict {
            Ok(range_len) => Some(range_len),
            Err(reason) => {
                if let Some((_, p)) = resume_paths.as_ref() {
                    let _ = std::fs::remove_file(p);
                    let _ = std::fs::remove_file(meta_path_for(p));
                }
                return Err(Fail::typed(
                    format!("{}; the partial download was discarded", reason),
                    "resume-invalid",
                ));
            }
        }
    } else {
        None
    };

    // --- final name (server-derived candidates sanitize via the brain) --
    let name = match plan.name.clone() {
        Some(n) => n,
        None => match raw_name_candidate(&final_url, content_disposition.as_deref()) {
            Some(raw) => sanitize_via_brain(raw, proxy),
            None => format!("fress-download-{}.bin", plan.id),
        },
    };

    // --- staging ownership ---------------------------------------------
    let (dest, part, mut file) = if let Some((candidate, p)) = resume_paths.as_ref() {
        // Continuing our own staging file. 206: append; 200: truncate the
        // same staging file and restart in place. Either way the file is
        // ours - never a neighbor candidate's.
        let f = if resuming {
            std::fs::OpenOptions::new()
                .append(true)
                .open(p)
                .map_err(|e| Fail::plain(format!("Cannot reopen partial download: {}", e)))?
        } else {
            std::fs::File::create(p)
                .map_err(|e| Fail::plain(format!("Cannot write file: {}", e)))?
        };
        (candidate.clone(), p.clone(), f)
    } else {
        // Fresh download: atomically reserve a staging file for ourselves
        // (create_new). Two downloads racing on the same filename must
        // never share one .part.
        let mut reserved: Option<(PathBuf, PathBuf, std::fs::File)> = None;
        for i in 0..10_000u32 {
            let candidate = dir.join(numbered_name(&name, i));
            let p = part_path_for(&candidate);
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
                Ok(f) => {
                    reserved = Some((candidate, p, f));
                    break;
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Fail::plain(format!("Cannot write file: {}", e))),
            }
        }
        reserved.ok_or_else(|| {
            Fail::plain("Could not reserve a staging file for the download".to_string())
        })?
    };
    // From here on this attempt owns `part`; a retry continues it.
    let staging_opened = part.clone();

    // Persist the origin's validators for the resume path.
    write_part_meta(&part, &plan.url, etag.as_deref(), last_modified.as_deref());

    let mut hasher = Sha256::new();
    // Seed the digest with the bytes from the earlier attempt so the final
    // hash covers the complete file, not just the resumed tail.
    if resuming {
        let mut existing =
            std::fs::File::open(&part).map_err(|e| Fail::plain(format!("Cannot read partial download: {}", e)))?;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = existing
                .read(&mut buf)
                .map_err(|e| Fail::plain(format!("Cannot read partial download: {}", e)))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }

    // --- streaming ------------------------------------------------------
    let mut stream = first.into_reader();
    let mut downloaded: u64 = already_have;
    let total = if remote_len > 0 { remote_len + already_have } else { 0 };
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let mut speed_samples: Vec<(Instant, u64)> = Vec::with_capacity(40);
    const SPEED_WINDOW: Duration = Duration::from_secs(4);
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        if cancel.load(Ordering::SeqCst) {
            // Deliberate stop: keep the .part file so the user can resume.
            drop(file);
            return Err(Fail::cancelled().with_staging(staging_opened.clone()));
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                if file.write_all(&buf[..n]).is_err() {
                    drop(file);
                    return Err(Fail::plain("Disk write failed".into())
                        .with_staging(staging_opened.clone()));
                }
                downloaded += n as u64;

                // Emit progress at most ~10x per second, with a truthful
                // CURRENT speed (4 s rolling window).
                if last_emit.elapsed() >= Duration::from_millis(100) {
                    speed_samples.retain(|(t, _)| t.elapsed() < SPEED_WINDOW);
                    speed_samples.push((Instant::now(), downloaded));
                    let speed = if let Some((oldest_t, oldest_b)) = speed_samples.first() {
                        let dt = oldest_t.elapsed().as_secs_f64();
                        if dt > 0.05 {
                            (downloaded.saturating_sub(*oldest_b)) as f64 / dt
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    };
                    let eta = if total > downloaded && speed > 0.0 {
                        (total - downloaded) as f64 / speed
                    } else {
                        0.0
                    };
                    let _ = proxy.send_event(super::Msg::DlProgress {
                        id: plan.id,
                        downloaded,
                        total,
                        speed_bps: speed,
                        eta_secs: eta,
                    });
                    last_emit = Instant::now();
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                // No bytes for a full minute: treat the connection as
                // dead. The .part stays on disk for the Resume button and
                // the brain's auto-retry.
                drop(file);
                return Err(Fail::typed("Connection stalled (no data for 60s)".into(), "network")
                    .with_staging(staging_opened.clone()));
            }
            Err(e) => {
                drop(file);
                return Err(Fail::typed(format!("Download interrupted: {}", e), "network")
                    .with_staging(staging_opened.clone()));
            }
        }
    }

    file.flush().map_err(|e| Fail::plain(e.to_string()))?;
    drop(file);

    // EOF is not completion: refuse to finalize a truncated file. The
    // .part survives so Resume can fetch exactly the missing bytes.
    let expected_downloaded: Option<u64> = match expected_range_len {
        Some(range_len) => Some(already_have + range_len),
        None if !resuming && remote_len > 0 => Some(remote_len),
        None => None,
    };
    if let Some(expected) = expected_downloaded {
        if downloaded != expected {
            return Err(Fail::plain(format!(
                "Download cut short: received {} of {} bytes before the connection closed. Resume to fetch the remaining bytes.",
                downloaded, expected
            ))
            .with_staging(staging_opened));
        }
    }

    let sha_hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
    Ok(Report {
        id: plan.id,
        outcome: "complete".into(),
        staging: Some(part.to_string_lossy().into()),
        name: Some(name),
        bytes: Some(downloaded),
        sha256: Some(sha_hex),
        message: None,
        kind: None,
    })
}
