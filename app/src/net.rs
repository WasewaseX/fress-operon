//! The https GET effector. The brain INSTRUCTS the host to fetch (the
//! `net_get` instruction envelope) — the host performs the request with
//! the exact client profile the original Fress core used: Fress UA,
//! 20 s overall timeout, 15 s connect timeout, at most 5 redirects.
//! Responses come back as JSON for the brain's continuation genes.

use serde::Serialize;
use std::io::Read;
use std::time::Duration;

pub const USER_AGENT: &str = "Fress/1.0.2-beta (+https://github.com/WasewaseX/Fress)";
/// Generous sanity cap for API JSON bodies (the 2 MiB text-fetch policy is
/// the BRAIN's decision, applied in fress_on_net).
const API_BODY_CAP: u64 = 64 * 1024 * 1024;
/// The body cap the brain's text policy works against; the host reads at
/// most one byte MORE than this so the brain can see `more`.
pub const TEXT_BODY_CAP: u64 = 2 * 1024 * 1024;

#[derive(Serialize, Clone)]
pub struct NetResp {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub more: Option<bool>,
}

impl NetResp {
    pub fn transport_err(msg: String) -> NetResp {
        // The original core mapped reqwest send errors to this prefix; the
        // brain passes it through verbatim.
        NetResp {
            ok: false,
            error: Some(format!("Network error: {}", msg)),
            status: None,
            final_url: None,
            content_length: None,
            body: None,
            more: None,
        }
    }
}

fn agent() -> Result<ureq::Agent, String> {
    Ok(ureq::AgentBuilder::new()
        .user_agent(USER_AGENT)
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(20))
        .redirects(5)
        .build())
}

/// GET `url` with optional headers. `text_mode` caps the read at
/// TEXT_BODY_CAP + 1 bytes and reports `more`; API mode caps at 64 MiB.
pub fn net_get(url: &str, headers: &[(String, String)], text_mode: bool) -> NetResp {
    let agent = match agent() {
        Ok(a) => a,
        Err(e) => return NetResp::transport_err(e),
    };
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_code, r)) => r,
        Err(e) => return NetResp::transport_err(e.to_string()),
    };
    let status = resp.status();
    let final_url = resp.get_url().to_string();
    let content_length = resp
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok());

    let cap = if text_mode {
        TEXT_BODY_CAP + 1
    } else {
        API_BODY_CAP
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut reader = resp.into_reader().take(cap);
    if let Err(e) = std::io::Read::read_to_end(&mut reader, &mut buf) {
        return NetResp::transport_err(e.to_string());
    }
    // read_to_end over take(cap) cannot exceed cap; more = we filled the cap
    // and the stream wanted more.
    let more = buf.len() as u64 >= cap;
    let body = String::from_utf8_lossy(&buf).into_owned();

    NetResp {
        ok: true,
        error: None,
        status: Some(status),
        final_url: Some(final_url),
        content_length,
        body: Some(body),
        more: Some(more),
    }
}
