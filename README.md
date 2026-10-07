# Fress Operon Edition

The [Fress](https://github.com/WasewaseX/Fress) directory app **ported to the
[operon](https://github.com/WasewaseX/operon-lang-dev) language** — with the
original React GUI shipped **byte-identical** and the entire backend decision
logic rewritten in operon.

This repo is a clean-room port: it never modifies the upstream Fress repo or
any clone of it. The frontend is built from a pinned upstream commit and
embedded as-is; the Tauri IPC core that the frontend talks to is replaced by
a light native host with the operon brain behind it.

```
┌───────────────────────────────────────────────────────────────┐
│  fress-operon.exe (native window, system webview)             │
│                                                               │
│  BODY (Rust host)                    BRAIN (op/fress.op)      │
│  ├─ wry/tao window + shim   ←──IPC──→ ├─ repo/URL validation  │
│  │  implementing the exact            ├─ resolver allowlist   │
│  │  Tauri IPC surface                 ├─ GitHub release fetch │
│  ├─ https GET effector                │  + prerelease filters │
│  ├─ download streaming worker         ├─ F-Droid resolution   │
│  │  (progress, sha256, resume,        ├─ filename policy      │
│  │   validators, cancel)              │  (Unicode-aware,      │
│  ├─ dialogs, openers                  │   reserved names)     │
│  └─ arch/version/downloads dir        ├─ retry policy         │
│                                       └─ checksum verdicts    │
│  embedded operon interpreter (VM lane, capability sandbox:     │
│  default-deny, exact read/write grants)                        │
└───────────────────────────────────────────────────────────────┘
```

## What is ported, what is preserved

| Original (Tauri Rust core) | Where it lives now |
|---|---|
| `fetch_latest_release` / `fetch_recent_releases` | operon: `fetch_latest_release` / `fetch_recent_releases` + `fress_on_net` |
| `fetch_text` (https-only, host allowlist, 2 MiB cap policy) | operon: `fetch_text` / `url_host` / `ALLOWED_HOSTS` |
| `fetch_fdroid_package` (suggestedVersionCode rules) | operon: `fetch_fdroid_package` / `fress_on_net` |
| `sanitize_filename` (Unicode-aware, Windows reserved names) | operon: `sanitize_filename` (via `char_category`) |
| RFC 5987 percent-decoding | operon: `percent_decode` (bytes builtins) + host candidate extraction |
| download auto-retry policy (2 network retries, 2 s/4 s) | operon: `dl_on_result` |
| checksum verification verdicts (mismatch = delete) | operon: `dl_on_result` |
| download job registry (ids) | operon: `STATE` |
| `start_download` streaming engine (Range resume, If-Range validators, Content-Range strict validation, 416 handling, truncation refusal, 10 Hz progress with 4 s rolling speed) | host: `app/src/worker.rs` (the same place the original put it) |
| `default_download_dir` / `host_arch` (GetNativeSystemInfo) / `app_version` | host (pure capabilities, no decisions) |
| **The GUI (React 19 + Tailwind 4 bundle)** | **preserved byte-identical** — built from pinned upstream `d090912` and served from the executable |

The IPC surface the frontend sees is exactly the original one: the same nine
commands, the same `download-progress` / `download-complete` /
`download-error` / `download-retrying` events, and `plugin:dialog|open`,
`plugin:opener|open_path` / `reveal_item_in_dir` / `open_url`,
`plugin:path|dirname`. A tiny shim (`app/src/shim.js`) provides the
`__TAURI_INTERNALS__` contract; the frontend bundle is untouched.

## Error strings

User-facing command errors (`No GitHub repository configured`, `GitHub rate
limit reached; try again in a few minutes`, `Only https URLs are allowed`,
`Host X is not on the resolver allowlist`, `Integrity check failed: …`,
`Download cut short: …`, …) are pinned byte-exact in the brain test suite.
Transport-internal detail inside `Network error: {}` / `Bad response: {}`
naturally reflects the host's HTTP stack wording.

## Build

```sh
# 1. frontend: checks out upstream Fress @ d090912 (scratch dir, upstream
#    untouched), npm ci + vite build, copies dist/ into app/frontend-dist/
bash scripts/build-frontend.sh

# 2. host + brain
cargo build --release        # Windows is the release target; a GUI-less
                             # machine needs webkit2gtk on Linux

# 3. headless brain tests (no network, no webview)
cargo test -p fresscore
```

## Honest notes (v1.0.2-beta parity)

- `app_version()` reports `1.0.2-beta`, so the in-app self-update sees the
  operon edition at parity with upstream `v1.0.2-beta` and stays quiet
  until upstream ships something newer.
- localStorage sits under `FressOperon/WebView2` (different webview origin
  than Tauri's), so the download-folder preference and theme start fresh
  once.
- The self-update check still reads `WasewaseX/Fress` releases (unchanged
  GUI logic), so a newer upstream release would be offered as an update —
  which installs the official build. That is the faithful behavior; a
  vault-hosted update lane for the operon edition is a future decision.
- Third-party attribution: this is NOT an official Fress release; origin is
  stated in NOTICE (preserved verbatim) per upstream's MIT terms.
