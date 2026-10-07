//! fresscore — Fress Operon Edition, desktop core.
//!
//! This is the operon port of Fress: the same "real native desktop app"
//! architecture proven in Youwee Operon Edition, but unlike youwee the GUI
//! is not rebuilt — the ORIGINAL Fress React frontend ships byte-identical
//! inside the webview, and this host replaces the Tauri IPC core behind it.
//!
//! Division of labor (mirrors op/fress.op's header):
//!   - operon (op/fress.op) is the BRAIN: URL/repo validation, the resolver
//!     host allowlist, GitHub/F-Droid response mapping, filename policy,
//!     retry policy, checksum verdicts. Everything that decides.
//!   - the host is the BODY: the webview window, the https GET effector,
//!     the streaming download worker (progress math, sha256, resume),
//!     dialogs, openers. Everything that acts.
//!
//! Protocol: every bridge call passes ONE JSON document and reads ONE JSON
//! document back — parsed and serialized with the language's own
//! json_parse/json_stringify (the core eats its own food, zero extra crates
//! here).

use operon::interp::{json_stringify, Caps};
use operon::tools::{self, Loaded, Opts};
use operon::value::Value;
use std::cell::RefCell;
use std::rc::Rc;

/// Capability grants the host gives the app. Default-deny, exactly like the
/// CLI launcher contract: the brain can read/write its own app dirs and
/// temp. No net grants — the brain INSTRUCTS the host to fetch (the
/// `net_get` instruction envelope), and the host, which grants the
/// capability, performs it. No run grants — nothing is spawned.
#[derive(Clone, Debug, Default)]
pub struct Grants {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub env: Vec<String>,
}

impl Grants {
    pub fn to_caps(&self) -> Result<Caps, String> {
        let mut caps = Caps::default();
        let mut grant = |kind: &str, v: &String| -> Result<(), String> {
            caps.add_grant(kind, v)
                .map_err(|s| format!("grant {} '{}': {}", kind, v, s.message))
        };
        for p in &self.read {
            grant("read", p)?;
        }
        for p in &self.write {
            grant("write", p)?;
        }
        for p in &self.env {
            grant("env", p)?;
        }
        Ok(caps)
    }
}

pub struct Runtime {
    l: Loaded,
    logs: Rc<RefCell<Vec<String>>>,
    entry_opts: Opts,
}

fn stress_str(s: &operon::value::Stress) -> String {
    format!("[{}] {} (brain line {})", s.kind, s.message, s.line)
}

impl Runtime {
    /// Load the operon brain (executes its top-level bindings), set the VM
    /// lane and the run-wide fuel pool, then invoke its `main` gene (boot
    /// selfcheck).
    pub fn boot(app_path: &str, grants: &Grants) -> Result<Runtime, String> {
        let logs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let caps = grants.to_caps()?;
        let opts = Opts {
            cell: None,
            variant: None,
            rna: None,
            entry: Some("main".to_string()),
            use_ires: false,
            frame: None,
            args: Vec::new(),
            quiet: true,
            caps,
            profile: false,
            spans: false,
            stdout_sink: Some(logs.clone()),
            use_vm: true,
        };
        let mut l = tools::load_file(app_path, &opts)?;
        // run-wide shared fuel pool (SPEC §9b): a runaway loop cannot
        // multiply the budget.
        l.interp.fuel_pool = Some(std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
            500_000_000,
        )));
        // the VM lane, default encoding — the lane the whole differential
        // suite and the compat matrix gate on.
        l.interp.vm = true;
        l.interp.vm_opt = 0;
        l.interp.vm_program = Some(operon::vm::VmProgram::default());
        tools::run_entry(&mut l, &opts).map_err(|s| stress_str(&s))?;
        Ok(Runtime {
            l,
            logs,
            entry_opts: opts,
        })
    }

    /// Call a brain gene with one JSON document argument; the return value
    /// comes back as JSON (both directions through the language's own JSON
    /// codec). Non-JSON text falls through as a bare string.
    pub fn call(&mut self, gene: &str, arg: Option<&str>) -> Result<String, String> {
        let genv = self.l.interp.global.clone();
        let args = match arg {
            Some(s) => vec![match operon::interp::json_parse(s) {
                Ok(v) => v,
                Err(_) => Value::Str(s.to_string()),
            }],
            None => Vec::new(),
        };
        match self.l.interp.call_named(&genv, gene, args, None) {
            Ok(v) => Ok(json_stringify(&v)),
            Err(s) => Err(stress_str(&s)),
        }
    }

    /// Drain the brain's promote() ring (diagnostics feed).
    pub fn drain_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut *self.logs.borrow_mut())
    }

    /// Re-run the entry gene (kept for future re-boot paths).
    pub fn reentry(&mut self) -> Result<String, String> {
        tools::run_entry(&mut self.l, &self.entry_opts)
            .map(|v| json_stringify(&v))
            .map_err(|s| stress_str(&s))
    }
}

/// JSON-escape a Rust string for hand-built host->brain payloads.
pub fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
