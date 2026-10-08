//! fress-operon — Fress Operon Edition host.
//!
//! A real native desktop window (wry/tao — the same system-webview engine
//! Tauri itself uses) rendering the ORIGINAL Fress React bundle
//! byte-identically, with the operon brain behind the IPC surface the
//! frontend expects. No localhost server, no browser tab.
//!
//! The main thread owns the operon Runtime (the interpreter is !Send by
//! design); every blocking effect (https GET, file streaming) happens on
//! worker threads that report back through the event-loop proxy. The brain
//! decides, the host acts.

// Windows release builds are GUI-subsystem apps: double-clicking the exe
// opens the app window ONLY — no terminal window, exactly like any other
// desktop app. Debug builds keep the console for development logging.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod net;
mod util;
mod worker;

use fresscore::{jstr, Grants, Runtime};
use serde_json::{json, Value as J};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use wry::{WebView, WebViewBuilder};

pub const APP_VERSION: &str = "1.0.3-beta";

pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self' https://api.github.com https://github.com https://f-droid.org; object-src 'none'; base-uri 'self'; form-action 'self'; frame-src 'none'";

// wry 0.46 registers the WebView2 custom-protocol interception filter for
// the DEFAULT scheme `http` (http://fress.*) — `with_https_scheme(true)` is
// required to serve it over https. Navigating to https://fress.localhost
// while the filter only covers http:// fell through to the network stack:
// Chromium resolves *.localhost to 127.0.0.1, finds nothing on port 443 and
// renders WebView2's "can't reach this page" error — the exact bug users saw
// in v1.0.2-beta. Stay on the default http scheme (Tauri v2 does the same
// with http://tauri.localhost); localhost origins are trusted contexts in
// Chromium, so crypto.subtle and friends keep working.
#[cfg(target_os = "windows")]
const INDEX_URL: &str = "http://fress.localhost/index.html";
#[cfg(not(target_os = "windows"))]
const INDEX_URL: &str = "fress://localhost/index.html";

/// Runs inside the webview after boot: the page is REALLY ours only when
/// the shim is alive AND the frontend bundle mounted its React root. A
/// WebView2 error page has neither. The host probes this after startup and
/// never lets a bare web error page face the user again.
const PAGE_PROBE_JS: &str =
    "JSON.stringify({shim: !!window.__TAURI_INTERNALS__, root: !!document.getElementById('root')})";

/// Commands whose decision logic lives in the operon brain.
const BRAIN_COMMANDS: &[&str] = &[
    "fetch_latest_release",
    "fetch_recent_releases",
    "fetch_text",
    "fetch_fdroid_package",
    "start_download",
    "cancel_download",
];

enum Msg {
    Invoke { id: u64, cmd: String, args: J },
    NetDone { invoke_id: u64, cmd: String, ctx: J, resp: net::NetResp, depth: u8 },
    Sanitize { raw: String, reply: std::sync::mpsc::SyncSender<String> },
    DlProgress { id: u32, downloaded: u64, total: u64, speed_bps: f64, eta_secs: f64 },
    DlDone { plan: worker::Plan, report: worker::Report },
    /// CI smoke: boot the whole stack headless-ish, then exit 0.
    Smoke,
    /// Fire the page probe (shim + React root check).
    ProbePage,
    /// Probe verdict came back from the webview callback.
    ProbeResult(String),
    /// Final page verdict: recover or fail loudly — never a silent death.
    ProbeFinal,
    /// Smoke fail-safe: exit(1) if the boot never reached a verdict.
    SmokeTimeout,
}

/// Main-thread state: the operon brain, the webview, and the download
/// cancel registry.
struct State {
    runtime: Runtime,
    webview: WebView,
    proxy: EventLoopProxy<Msg>,
    cancels: HashMap<u32, Arc<AtomicBool>>,
    downloads_dir: String,
    /// Page-load verdict (shim + React root seen).
    page_ok: Option<bool>,
    /// Smoke-mode brain verdict.
    brain_ok: Option<bool>,
    /// --smoke mode flag.
    smoke: bool,
    /// One automatic reload was already tried.
    reloaded: bool,
}

impl State {
    fn resolve(&self, id: u64, ok: bool, value: &str) {
        let _ = self.webview.evaluate_script(&format!(
            "window.__FRESS_HOST__.resolveInvoke({},{},{});",
            id,
            ok,
            jstr(value)
        ));
    }

    fn emit(&self, event: &str, payload: &J) {
        let _ = self.webview.evaluate_script(&format!(
            "window.__FRESS_HOST__.emitEvent({},{});",
            jstr(event),
            jstr(&payload.to_string())
        ));
    }

    fn brain_call(&mut self, gene: &str, arg_json: &str) -> Result<J, String> {
        let out = self.runtime.call(gene, Some(arg_json))?;
        // GUI-subsystem builds have no stderr — and println to a dead
        // console panics. Route brain logs to a file instead (best-effort).
        for line in self.runtime.drain_logs() {
            host_log(&line);
        }
        serde_json::from_str(&out).map_err(|e| {
            format!("brain response is not a JSON object: {} :: {}", e, out)
        })
    }

    /// Fire the page probe: the verdict comes back asynchronously through
    /// Msg::ProbeResult (wry serializes the evaluation result into the
    /// callback; evaluate_script itself returns nothing).
    fn request_probe(&self) {
        let proxy = self.proxy.clone();
        let r = self.webview.evaluate_script_with_callback(PAGE_PROBE_JS, move |raw| {
            let _ = proxy.send_event(Msg::ProbeResult(raw));
        });
        if r.is_err() {
            host_log("page probe: evaluate_script_with_callback failed");
        }
    }

    /// Parse a probe reply: Some(true) = our page is really up (shim alive
    /// + React root mounted), Some(false) = webview answered but it is NOT
    /// our page (e.g. a WebView2 error page), None = unparseable.
    fn parse_probe(raw: &str) -> Option<bool> {
        let mut v: J = serde_json::from_str(raw.trim()).ok()?;
        if v.is_string() {
            v = serde_json::from_str(v.as_str().unwrap()).ok()?;
        }
        Some(v["shim"] == json!(true) && v["root"] == json!(true))
    }

    /// Smoke-mode finish: write the result file next to the exe (CI reads
    /// it — a GUI-subsystem exe has no stdout pipes) and exit.
    fn finish_smoke(&self, ok: bool) -> ! {
        let brain = self.brain_ok == Some(true);
        let page = self.page_ok == Some(true);
        let line = format!(
            "{} brain={} page={}",
            if ok { "SMOKE OK" } else { "SMOKE FAIL" },
            if brain { "ok" } else { "fail" },
            if page { "PAGE OK" } else { "PAGE FAIL" },
        );
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let _ = std::fs::write(dir.join("smoke-result.txt"), &line);
            }
        }
        println!("{}", line);
        std::process::exit(if ok { 0 } else { 1 });
    }

    /// Route a brain response envelope: final result / error / net
    /// instruction / download plan / cancel.
    fn route_brain(&mut self, invoke_id: u64, cmd: &str, brain: J, depth: u8) {
        if brain["ok"] == json!(false) {
            let msg = brain["error"].as_str().unwrap_or("Unknown error").to_string();
            self.resolve(invoke_id, false, &msg);
            return;
        }
        if let Some(result) = brain.get("result") {
            self.resolve(invoke_id, true, &result.to_string());
            return;
        }
        if brain["need"] == json!("net_get") {
            if depth >= 4 {
                self.resolve(invoke_id, false, "Brain exceeded the net instruction chain");
                return;
            }
            let url = brain["url"].as_str().unwrap_or_default().to_string();
            let headers: Vec<(String, String)> = brain["headers"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let ctx = brain["ctx"].clone();
            let proxy = self.proxy.clone();
            let text_mode = cmd == "fetch_text";
            let cmd_owned = cmd.to_string();
            std::thread::spawn(move || {
                let resp = net::net_get(&url, &headers, text_mode);
                let _ = proxy.send_event(Msg::NetDone {
                    invoke_id,
                    cmd: cmd_owned,
                    ctx,
                    resp,
                    depth,
                });
            });
            return;
        }
        if let Some(plan_v) = brain.get("plan") {
            self.start_download(invoke_id, plan_v.clone());
            return;
        }
        if brain["cancel"] == json!(true) {
            // cancel_download: flip the worker's cancel flag (silent on
            // unknown ids, exactly like the original).
            if let Some(idv) = brain.get("_cancel_id").and_then(|v| v.as_u64()) {
                if let Some(flag) = self.cancels.get(&(idv as u32)) {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            self.resolve(invoke_id, true, "null");
            return;
        }
        self.resolve(
            invoke_id,
            false,
            &format!("Unhandled brain response: {}", brain),
        );
    }

    fn start_download(&mut self, invoke_id: u64, plan_v: J) {
        let plan: worker::Plan = match serde_json::from_value(plan_v) {
            Ok(p) => p,
            Err(e) => {
                self.resolve(invoke_id, false, &format!("Bad download plan: {}", e));
                return;
            }
        };
        // The original created the download folder synchronously in the
        // command, so creation failures surface to the invoke.
        if let Err(e) = std::fs::create_dir_all(&plan.dir) {
            self.resolve(
                invoke_id,
                false,
                &format!("Cannot create download folder: {}", e),
            );
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.insert(plan.id, cancel.clone());
        let proxy = self.proxy.clone();
        let plan2 = plan.clone();
        std::thread::spawn(move || {
            let report = worker::run_download(plan2.clone(), cancel, proxy.clone());
            let _ = proxy.send_event(Msg::DlDone { plan: plan2, report });
        });
        self.resolve(invoke_id, true, &plan.id.to_string());
    }

    fn handle_invoke(&mut self, id: u64, cmd: String, args: J) {
        match cmd.as_str() {
            "app_version" => self.resolve(id, true, &json!(APP_VERSION).to_string()),
            "host_arch" => self.resolve(id, true, &json!(host_arch()).to_string()),
            "default_download_dir" => {
                self.resolve(id, true, &json!(self.downloads_dir.clone()).to_string())
            }
            "plugin:path|dirname" => {
                let p = args["path"].as_str().unwrap_or("");
                self.resolve(id, true, &json!(host_dirname(p)).to_string());
            }
            "plugin:dialog|open" => {
                let opts = &args["options"];
                let title = opts["title"].as_str().unwrap_or("");
                let mut dialog = rfd::FileDialog::new();
                if !title.is_empty() {
                    dialog = dialog.set_title(title);
                }
                let picked = dialog.pick_folder();
                let value = match picked {
                    Some(p) => json!(p.to_string_lossy()),
                    None => J::Null,
                };
                self.resolve(id, true, &value.to_string());
            }
            "plugin:opener|open_path" => {
                let p = args["path"].as_str().unwrap_or("");
                match os_open(p) {
                    Ok(()) => self.resolve(id, true, "null"),
                    Err(e) => self.resolve(id, false, &e),
                }
            }
            "plugin:opener|open_url" => {
                let u = args["url"].as_str().unwrap_or("");
                match os_open(u) {
                    Ok(()) => self.resolve(id, true, "null"),
                    Err(e) => self.resolve(id, false, &e),
                }
            }
            "plugin:opener|reveal_item_in_dir" => {
                let paths = args["paths"].as_array().cloned().unwrap_or_default();
                let first = paths
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match os_reveal(&first) {
                    Ok(()) => self.resolve(id, true, "null"),
                    Err(e) => self.resolve(id, false, &e),
                }
            }
            c if BRAIN_COMMANDS.contains(&c) => {
                let snake = snake_case_keys(args);
                let mut arg = snake;
                if c == "cancel_download" {
                    // the host effect (flag flip) rides the envelope back
                    arg["_cancel_id"] = arg["id"].clone();
                }
                if c == "start_download" {
                    arg["downloads_dir"] = json!(self.downloads_dir.clone());
                }
                match self.brain_call(c, &arg.to_string()) {
                    Ok(brain) => self.route_brain(id, c, brain, 0),
                    Err(e) => self.resolve(id, false, &e),
                }
            }
            _ => self.resolve(id, false, "Unknown command"),
        }
    }

    fn handle_dl_done(&mut self, plan: worker::Plan, report: worker::Report) {
        let report_json = serde_json::to_value(&report).unwrap_or(J::Null);
        let brain = match self.brain_call("dl_on_result", &report_json.to_string()) {
            Ok(b) => b,
            Err(e) => {
                self.cancels.remove(&plan.id);
                self.emit(
                    "download-error",
                    &json!({"id": report.id, "message": e}),
                );
                return;
        }
        };
        match brain["action"].as_str().unwrap_or("") {
            "finalize" => {
                self.cancels.remove(&plan.id);
                let part = PathBuf::from(brain["part"].as_str().unwrap_or_default());
                let name = report
                    .name
                    .clone()
                    .unwrap_or_else(|| "download.bin".to_string());
                let dir = PathBuf::from(&plan.dir);
                // Re-check for a late collision: another file with this
                // exact name may have appeared while the bytes were
                // streaming (rename silently replaces on Unix).
                let dest = if dir.join(&name).exists() {
                    util::unique_path(&dir, &name)
                } else {
                    dir.join(&name)
                };
                if let Err(e) = std::fs::rename(&part, &dest) {
                    let _ = std::fs::remove_file(util::meta_path_for(&part));
                    self.emit(
                        "download-error",
                        &json!({"id": report.id, "message": format!("Cannot finalize the download: {}", e)}),
                    );
                    return;
                }
                // The staging file is gone; its validator sidecar has no
                // reason to outlive it.
                let _ = std::fs::remove_file(util::meta_path_for(&part));
                let mut payload = brain["emit"]["payload"].clone();
                payload["path"] = json!(dest.to_string_lossy());
                self.emit(
                    brain["emit"]["event"].as_str().unwrap_or("download-complete"),
                    &payload,
                );
            }
            "delete_and_error" => {
                self.cancels.remove(&plan.id);
                let part = PathBuf::from(brain["part"].as_str().unwrap_or_default());
                let _ = std::fs::remove_file(&part);
                let _ = std::fs::remove_file(util::meta_path_for(&part));
                self.emit("download-error", &brain["payload"].clone());
            }
            "error" => {
                self.cancels.remove(&plan.id);
                self.emit("download-error", &brain["payload"].clone());
            }
            "retry" => {
                // The cancel registry entry stays alive across retries
                // (registered at start, removed after the final result).
                self.emit("download-retrying", &brain["payload"].clone());
                let mut plan2 = plan.clone();
                plan2.resume = true;
                if let Some(s) = &report.staging {
                    if !s.is_empty() {
                        plan2.staging = Some(s.clone());
                    }
                }
                let delay = brain["delay_secs"].as_u64().unwrap_or(2);
                let proxy = self.proxy.clone();
                let cancel = self
                    .cancels
                    .get(&plan.id)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(delay));
                    let report = worker::run_download(plan2.clone(), cancel, proxy.clone());
                    let _ = proxy.send_event(Msg::DlDone { plan: plan2, report });
                });
            }
            _ => {
                self.cancels.remove(&plan.id);
                self.emit(
                    "download-error",
                    &json!({"id": report.id, "message": "Unknown brain download action"}),
                );
            }
        }
    }
}

fn snake_case_keys(v: J) -> J {
    match v {
        J::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                out.insert(to_snake_case(&k), snake_case_keys(val));
            }
            J::Object(out)
        }
        J::Array(a) => J::Array(a.into_iter().map(snake_case_keys).collect()),
        other => other,
    }
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn host_dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return path.to_string();
    }
    if let Some(pos) = trimmed.rfind(['/', '\\']) {
        if pos == 0 {
            return trimmed[..1].to_string();
        }
        return trimmed[..pos].to_string();
    }
    ".".to_string()
}

fn host_arch() -> String {
    #[cfg(target_os = "windows")]
    {
        if let Some(arch) = windows_machine_arch() {
            return arch.to_string();
        }
    }
    std::env::consts::ARCH.to_string()
}

#[cfg(target_os = "windows")]
fn windows_machine_arch() -> Option<&'static str> {
    // The architecture of the MACHINE, not of this process: an x64 build
    // under emulation on Windows-on-ARM must still report aarch64 so the
    // pickers offer native ARM installers.
    #[repr(C)]
    struct SystemInfo {
        w_processor_architecture: u16,
        w_reserved: u16,
        dw_page_size: u32,
        lp_minimum_application_address: *mut core::ffi::c_void,
        lp_maximum_application_address: *mut core::ffi::c_void,
        dw_active_processor_mask: usize,
        dw_number_of_processors: u32,
        dw_processor_type: u32,
        dw_allocation_granularity: u32,
        w_processor_level: u16,
        w_processor_revision: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetNativeSystemInfo(lp_system_info: *mut SystemInfo);
    }
    const PROCESSOR_ARCHITECTURE_INTEL: u16 = 0;
    const PROCESSOR_ARCHITECTURE_AMD64: u16 = 9;
    const PROCESSOR_ARCHITECTURE_ARM64: u16 = 12;
    let mut si: SystemInfo = unsafe { core::mem::zeroed() };
    unsafe { GetNativeSystemInfo(&mut si) };
    match si.w_processor_architecture {
        PROCESSOR_ARCHITECTURE_ARM64 => Some("aarch64"),
        PROCESSOR_ARCHITECTURE_AMD64 => Some("x86_64"),
        PROCESSOR_ARCHITECTURE_INTEL => Some("x86"),
        _ => None,
    }
}

fn default_download_dir() -> String {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .to_string_lossy()
        .to_string()
}

fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(base) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(base).join("FressOperon");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".local/share/FressOperon");
        return p;
    }
    PathBuf::from(".fress-operon")
}

/// Materialize the brain next to the app data (the runtime reads it from
/// disk; grants are path-based).
fn write_brain(app_root: &PathBuf) -> String {
    let path = app_root.join("fress.op");
    if !path.exists() {
        std::fs::write(&path, include_str!("../../op/fress.op"))
            .expect("write fress.op brain");
    }
    path.to_string_lossy().to_string()
}

// ---- openers (no extra crates) -----------------------------------------

#[cfg(target_os = "windows")]
fn os_open(target: &str) -> Result<(), String> {
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut core::ffi::c_void,
            verb: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> usize;
    }
    let verb = wide("open");
    let file = wide(target);
    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            5,
        )
    };
    if code > 32 {
        Ok(())
    } else {
        Err(format!("Could not open {}", target))
    }
}

#[cfg(target_os = "windows")]
fn os_reveal(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("explorer")
        .arg(format!("/select,{}", path))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "windows"))]
fn os_open(target: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(target)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "windows"))]
fn os_reveal(path: &str) -> Result<(), String> {
    let p = PathBuf::from(path);
    match p.parent() {
        Some(dir) => os_open(&dir.to_string_lossy()),
        None => os_open(path),
    }
}

// ---- embedded frontend --------------------------------------------------

static DIST: include_dir::Dir = include_dir::include_dir!("$CARGO_MANIFEST_DIR/frontend-dist");

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn serve(
    _id: wry::WebViewId,
    request: wry::http::Request<Vec<u8>>,
) -> wry::http::Response<std::borrow::Cow<'static, [u8]>> {
    let uri = request.uri().to_string();
    // Windows serves custom protocols over https://fress.localhost/<path>;
    // other platforms over fress://localhost/<path>.
    let without_scheme = uri
        .strip_prefix("https://fress.localhost")
        .or_else(|| uri.strip_prefix("http://fress.localhost"))
        .or_else(|| uri.strip_prefix("fress://localhost"))
        .unwrap_or(&uri);
    let mut path = without_scheme.split(['?', '#']).next().unwrap_or("").to_string();
    while path.starts_with('/') {
        path = path[1..].to_string();
    }
    if path.is_empty() {
        path = "index.html".to_string();
    }
    let decoded = util::percent_decode(&path);
    let body = DIST
        .get_file(&decoded)
        .map(|f| f.contents().to_vec());
    match body {
        Some(bytes) => wry::http::Response::builder()
            .status(200)
            .header("Content-Type", mime_for(&decoded))
            .header("Content-Security-Policy", CSP)
            .body(std::borrow::Cow::Owned(bytes))
            .unwrap(),
        None => wry::http::Response::builder()
            .status(404)
            .header("Content-Type", "text/plain; charset=utf-8")
            .header("Content-Security-Policy", CSP)
            .body(std::borrow::Cow::Owned(b"not found".to_vec()))
            .unwrap(),
    }
}

/// Best-effort host log (GUI-subsystem apps have no console; a real file
/// also gives users something to send when reporting a problem).
fn host_log(line: &str) {
    use std::io::Write;
    let path = app_data_dir().join("fress-host.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", line);
    }
}

/// Native error dialog — never let the user face a bare web error page or
/// an instantly-vanishing console panic again.
#[cfg(target_os = "windows")]
fn msgbox(title: &str, text: &str) {
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            hwnd: *mut core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            utype: u32,
        ) -> i32;
    }
    const MB_ICONERROR: u32 = 0x10;
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide(title).as_ptr(),
            MB_ICONERROR,
        )
    };
}

#[cfg(not(target_os = "windows"))]
fn msgbox(title: &str, text: &str) {
    host_log(&format!("{}: {}", title, text));
}

/// Release builds are GUI-subsystem apps (no console). CI's --smoke run
/// re-attaches the parent console best-effort so interactive runs still
/// show output; the authoritative smoke verdict goes to a result file.
#[cfg(target_os = "windows")]
fn attach_parent_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(target_os = "windows"))]
fn attach_parent_console() {}

fn main() {
    let smoke = std::env::args().any(|a| a == "--smoke");
    if smoke {
        attach_parent_console();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let _ = std::fs::remove_file(dir.join("smoke-result.txt"));
            }
        }
    }

    // Release/Windows: a panic used to vanish with the process ("showed
    // unbelievable nothing"). Surface it in a native dialog instead.
    #[cfg(all(target_os = "windows", not(debug_assertions)))]
    {
        std::panic::set_hook(Box::new(|info| {
            msgbox(
                "Fress — unexpected error",
                &format!("Fress hit an unexpected error and must close.\n\n{}", info),
            );
        }));
    }

    let event_loop: EventLoop<Msg> = EventLoopBuilder::<Msg>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // The original window config: 1280x840, min 900x600, resizable,
    // centered, title "Fress".
    let window = match WindowBuilder::new()
        .with_title("Fress")
        .with_inner_size(LogicalSize::new(1280.0, 840.0))
        .with_min_inner_size(LogicalSize::new(900.0, 600.0))
        .with_resizable(true)
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => {
            let m = format!("Fress could not create its window.\n\n{}", e);
            msgbox("Fress — cannot start", &m);
            std::process::exit(1);
        }
    };

    if let Some(monitor) = window.current_monitor().or_else(|| window.primary_monitor()) {
        let ms = monitor.size();
        let ws = window.outer_size();
        let scale = window.scale_factor();
        let x = (ms.width as f64 - ws.width as f64) / 2.0 / scale;
        let y = (ms.height as f64 - ws.height as f64) / 2.0 / scale;
        window.set_outer_position(tao::dpi::LogicalPosition::new(x.max(0.0), y.max(0.0)));
    }

    // App data: the brain gets read/write grants exactly there; the
    // webview keeps its localStorage (download-folder preference, theme)
    // in a stable per-user place.
    let app_root = app_data_dir();
    let _ = std::fs::create_dir_all(&app_root);
    let webview_data = app_root.join("WebView2");

    let grants = Grants {
        read: vec![app_root.to_string_lossy().to_string()],
        write: vec![app_root.to_string_lossy().to_string()],
        env: vec![],
    };
    let brain_path = write_brain(&app_root);
    let runtime = match fresscore::Runtime::boot(&brain_path, &grants) {
        Ok(r) => r,
        Err(e) => {
            let m = format!(
                "The operon brain failed to boot.\n\n{}\n\nTry deleting the folder:\n{}",
                e,
                app_root.display()
            );
            msgbox("Fress — cannot start", &m);
            std::process::exit(1);
        }
    };

    let downloads_dir = default_download_dir();

    // The IPC handler only parses and forwards through the proxy (it runs
    // inside the webview pump; it must not touch the non-Send runtime).
    let ipc_proxy = proxy.clone();
    let shim = include_str!("shim.js").to_string();
    let mut web_context = wry::WebContext::new(Some(webview_data));
    let webview = match WebViewBuilder::with_web_context(&mut web_context)
        .with_url(INDEX_URL)
        .with_initialization_script(&shim)
        .with_custom_protocol("fress".into(), serve)
        .with_ipc_handler(move |request: wry::http::Request<String>| {
            let body = request.body();
            if let Ok(msg) = serde_json::from_str::<J>(body) {
                if msg["t"] == json!("invoke") {
                    let _ = ipc_proxy.send_event(Msg::Invoke {
                        id: msg["id"].as_u64().unwrap_or(0),
                        cmd: msg["cmd"].as_str().unwrap_or_default().to_string(),
                        args: msg["args"].clone(),
                    });
                }
            }
        })
        .build_as_child(&window)
    {
        Ok(w) => w,
        Err(e) => {
            let m = format!(
                "Fress needs the Microsoft WebView2 Runtime to render its window,\nand it is missing or broken on this machine.\n\n{}\n\nFix: install the Evergreen Runtime (free, ~2 minutes):\nhttps://go.microsoft.com/fwlink/p/?LinkId=2124703\nthen start Fress again.",
                e
            );
            msgbox("Fress — WebView2 Runtime required", &m);
            std::process::exit(1);
        }
    };

    let mut state = State {
        runtime,
        webview,
        proxy,
        cancels: HashMap::new(),
        downloads_dir,
        page_ok: None,
        brain_ok: None,
        smoke,
        reloaded: false,
    };

    // Page-load verification: after the boot settles, probe the webview for
    // OUR page (shim alive + React root mounted). The v1.0.2-beta bug (a
    // WebView2 "can't reach this page" error where the app should be) passed
    // CI because nothing verified the page — this closes that hole for good.
    {
        let probe_proxy = state.proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(4));
            let _ = probe_proxy.send_event(Msg::ProbePage);
            std::thread::sleep(std::time::Duration::from_secs(5));
            let _ = probe_proxy.send_event(Msg::ProbeFinal);
        });
    }

    // --smoke: boot the full stack (brain + webview + embedded frontend),
    // prove the window AND the page come up, then exit 0. Used by CI on
    // real Windows. The fail-safe timer guarantees the run always ends.
    if smoke {
        let smoke_proxy = state.proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(6));
            let _ = smoke_proxy.send_event(Msg::Smoke);
            std::thread::sleep(std::time::Duration::from_secs(9));
            let _ = smoke_proxy.send_event(Msg::SmokeTimeout);
        });
    }

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event, .. } => {
                if let WindowEvent::CloseRequested = event {
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(msg) => match msg {
                Msg::Smoke => {
                    // Record the brain verdict; the probe timers own the
                    // exit so the page result is always included.
                    let boot = state
                        .brain_call("main", "{}")
                        .unwrap_or_else(|e| json!({"ok": false, "error": e}));
                    state.brain_ok = Some(boot["ok"] == json!(true));
                    println!("SMOKE brain={}", boot);
                    if state.page_ok == Some(true) {
                        state.finish_smoke(state.brain_ok == Some(true));
                    }
                }
                Msg::ProbePage => {
                    state.request_probe();
                }
                Msg::ProbeResult(raw) => match State::parse_probe(&raw) {
                    Some(true) => {
                        state.page_ok = Some(true);
                        if state.smoke && state.brain_ok.is_some() {
                            state.finish_smoke(state.brain_ok == Some(true));
                        }
                    }
                    Some(false) => {
                        // One automatic recovery attempt in normal mode:
                        // re-navigate once (handles a startup race),
                        // re-probe, then the final verdict decides.
                        if !state.smoke && !state.reloaded {
                            state.reloaded = true;
                            let _ = state.webview.evaluate_script("location.reload()");
                            let retry = state.proxy.clone();
                            std::thread::spawn(move || {
                                std::thread::sleep(std::time::Duration::from_secs(3));
                                let _ = retry.send_event(Msg::ProbePage);
                            });
                        }
                    }
                    None => {
                        // The webview answered but the probe value was not
                        // parseable — only possible if our page never ran.
                        host_log(&format!("page probe: unparseable reply: {}", raw));
                    }
                },
                Msg::ProbeFinal => {
                    if state.page_ok != Some(true) {
                        if state.smoke {
                            state.finish_smoke(false);
                        }
                        let m = format!(
                            "Fress started but its interface could not load.\n\nThis is almost always an outdated or broken Microsoft\nWebView2 Runtime.\n\nFix: install the Evergreen Runtime (free, ~2 minutes):\nhttps://go.microsoft.com/fwlink/p/?LinkId=2124703\nthen start Fress again.\n\nTechnical details were written to:\n{}",
                            app_data_dir().join("fress-host.log").display()
                        );
                        msgbox("Fress — interface failed to load", &m);
                        std::process::exit(1);
                    }
                    if state.smoke && state.brain_ok == Some(true) {
                        state.finish_smoke(true);
                    }
                }
                Msg::SmokeTimeout => {
                    // Boot never reached a verdict — always fail loudly.
                    state.finish_smoke(false);
                }
                Msg::Invoke { id, cmd, args } => state.handle_invoke(id, cmd, args),
                Msg::NetDone { invoke_id, cmd, ctx, resp, depth } => {
                    let resp_json = serde_json::to_value(&resp).unwrap_or(J::Null);
                    let arg = json!({"ctx": ctx, "resp": resp_json});
                    match state.brain_call("fress_on_net", &arg.to_string()) {
                        Ok(brain) => state.route_brain(invoke_id, &cmd, brain, depth + 1),
                        Err(e) => state.resolve(invoke_id, false, &e),
                    }
                }
                Msg::Sanitize { raw, reply } => {
                    let name = state
                        .brain_call("sanitize", &json!({"name": raw}).to_string())
                        .ok()
                        .and_then(|b| b["name"].as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "download.bin".to_string());
                    let _ = reply.send(name);
                }
                Msg::DlProgress { id, downloaded, total, speed_bps, eta_secs } => {
                    state.emit(
                        "download-progress",
                        &json!({"id": id, "downloaded": downloaded, "total": total,
                                "speed_bps": speed_bps, "eta_secs": eta_secs}),
                    );
                }
                Msg::DlDone { plan, report } => state.handle_dl_done(plan, report),
            },
            _ => {}
        }
    })
}
