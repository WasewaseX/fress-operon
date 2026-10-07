//! Headless brain tests — the operon program in op/fress.op carries the
//! Fress backend's DECISION logic; this suite pins that logic against the
//! original Rust core's behavior (error strings, filters, clamps, policies,
//! verdicts). No network, no webview: net responses are injected as the
//! host would deliver them.

use fresscore::{Grants, Runtime};
use serde_json::{json, Value as J};
use std::path::PathBuf;

fn temp_root(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "fresscore-test-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn boot(tag: &str) -> Runtime {
    let root = temp_root(tag);
    let op_path = root.join("fress.op");
    std::fs::write(&op_path, include_str!("../../op/fress.op")).unwrap();
    let g = Grants {
        read: vec![root.to_string_lossy().to_string()],
        write: vec![root.to_string_lossy().to_string()],
        env: vec![],
    };
    Runtime::boot(op_path.to_str().unwrap(), &g).expect("brain boot")
}

fn call(rt: &mut Runtime, gene: &str, arg: &J) -> J {
    let out = rt
        .call(gene, Some(&arg.to_string()))
        .unwrap_or_else(|e| panic!("{}/{}: {}", gene, arg, e));
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("{}: bad JSON {}: {}", gene, out, e))
}

fn err_of(v: &J) -> String {
    v["error"].as_str().unwrap_or_default().to_string()
}

// ---------------------------------------------------------------- boot --

#[test]
fn boots_and_selfchecks() {
    let mut rt = boot("boot");
    let main = call(&mut rt, "main", &json!({}));
    assert_eq!(main["app"], json!("fress-operon"));
    assert_eq!(main["version"], json!("1.0.2-beta"));
    assert_eq!(main["allowlist"], json!(21));
}

// -------------------------------------------------------------- latest --

fn gh_release_json() -> J {
    json!({
        "tag_name": "v1.2.3",
        "name": "  Release 1.2.3  ",
        "published_at": "2026-01-02T03:04:05Z",
        "html_url": "https://github.com/owner/repo/releases/tag/v1.2.3",
        "prerelease": false,
        "assets": [
            {"name": "app.exe", "size": 1000, "browser_download_url": "https://github.com/owner/repo/releases/download/v1.2.3/app.exe"},
            {"name": "", "size": 1, "browser_download_url": "https://x/empty-name"},
            {"name": "no-url", "size": 1, "browser_download_url": ""}
        ]
    })
}

fn net_resp(status: u16, body: &str) -> J {
    json!({"ok": true, "status": status, "final_url": "https://final", "content_length": 100, "body": body, "more": false})
}

#[test]
fn latest_release_happy_path_and_errors() {
    let mut rt = boot("latest");

    // empty repo (after trim + slash-strip) -> the original's message
    let e = call(&mut rt, "fetch_latest_release", &json!({"repo": " /// "}));
    assert_eq!(err_of(&e), "No GitHub repository configured");

    // instruction shape: URL, Accept header, continuation ctx
    let inst = call(
        &mut rt,
        "fetch_latest_release",
        &json!({"repo": "/owner/repo/"}),
    );
    assert_eq!(inst["ok"], json!(true));
    assert_eq!(inst["need"], json!("net_get"));
    assert_eq!(
        inst["url"],
        json!("https://api.github.com/repos/owner/repo/releases/latest")
    );
    assert_eq!(inst["headers"]["Accept"], json!("application/vnd.github+json"));
    assert_eq!(inst["ctx"]["then"], json!("latest"));

    // happy path mapping
    let r = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": inst["ctx"], "resp": net_resp(200, &gh_release_json().to_string())}),
    );
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["result"]["tag"], json!("v1.2.3"));
    assert_eq!(r["result"]["name"], json!("Release 1.2.3"), "name is trimmed");
    assert_eq!(
        r["result"]["assets"].as_array().unwrap().len(),
        1,
        "empty-name and empty-url assets are dropped"
    );
    assert_eq!(
        r["result"]["assets"][0]["download_url"],
        json!("https://github.com/owner/repo/releases/download/v1.2.3/app.exe")
    );

    // status errors, byte-exact strings from the original core
    for (status, expected) in [
        (404u16, "No stable release found for this project"),
        (429, "GitHub rate limit reached; try again in a few minutes"),
        (403, "GitHub rate limit reached; try again in a few minutes"),
        (500, "GitHub returned HTTP 500"),
    ] {
        let e = call(
            &mut rt,
            "fress_on_net",
            &json!({"ctx": {"then": "latest", "repo": "owner/repo"}, "resp": net_resp(status, "{}")}),
        );
        assert_eq!(err_of(&e), expected, "status {}", status);
    }

    // bad JSON body
    let e = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "latest", "repo": "owner/repo"}, "resp": net_resp(200, "not json")}),
    );
    assert_eq!(err_of(&e), "Bad response: invalid JSON");

    // tagless release object
    let e = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "latest", "repo": "owner/repo"}, "resp": net_resp(200, "{\"name\":\"x\"}")}),
    );
    assert_eq!(err_of(&e), "Unexpected GitHub response");

    // transport failure passes through the host's "Network error: ..." text
    let e = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "latest", "repo": "owner/repo"},
                "resp": {"ok": false, "error": "Network error: timed out"}}),
    );
    assert_eq!(err_of(&e), "Network error: timed out");
}

// -------------------------------------------------------------- recent --

#[test]
fn recent_releases_filter_and_clamp() {
    let mut rt = boot("recent");

    let inst = call(
        &mut rt,
        "fetch_recent_releases",
        &json!({"repo": "owner/repo", "count": 50, "include_flagged": true}),
    );
    assert_eq!(inst["url"], json!("https://api.github.com/repos/owner/repo/releases?per_page=30"), "count clamps to 30");

    let inst = call(
        &mut rt,
        "fetch_recent_releases",
        &json!({"repo": "owner/repo"}),
    );
    assert_eq!(inst["url"], json!("https://api.github.com/repos/owner/repo/releases?per_page=20"), "default count 20");

    let inst = call(
        &mut rt,
        "fetch_recent_releases",
        &json!({"repo": "owner/repo", "count": 0}),
    );
    assert_eq!(inst["url"], json!("https://api.github.com/repos/owner/repo/releases?per_page=1"), "count clamps to 1");

    // prerelease filtering: stable kept, prerelease dropped; flagged keeps all
    let body = json!([
        {"tag_name": "v2", "name": "two", "prerelease": false, "assets": []},
        {"tag_name": "v3b", "name": "beta", "prerelease": true, "assets": []},
        {"tag_name": "", "name": "tagless", "prerelease": false, "assets": []}
    ])
    .to_string();

    let r = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "recent", "repo": "owner/repo", "flagged": false},
                "resp": net_resp(200, &body)}),
    );
    let list = r["result"].as_array().unwrap();
    assert_eq!(list.len(), 1, "prereleases and tagless entries are filtered");
    assert_eq!(list[0]["tag"], json!("v2"));

    let r = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "recent", "repo": "owner/repo", "flagged": true},
                "resp": net_resp(200, &body)}),
    );
    let list = r["result"].as_array().unwrap();
    assert_eq!(list.len(), 2, "include_flagged keeps flagged releases");
    assert_eq!(list[1]["tag"], json!("v3b"));

    // non-array body
    let e = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "recent", "repo": "owner/repo", "flagged": false},
                "resp": net_resp(200, "{}")}),
    );
    assert_eq!(err_of(&e), "Unexpected GitHub response");
}

// ---------------------------------------------------------------- text --

#[test]
fn fetch_text_policy() {
    let mut rt = boot("text");

    let e = call(&mut rt, "fetch_text", &json!({"url": "ftp://download.kde.org/x"}));
    assert_eq!(err_of(&e), "Only https URLs are allowed");

    let e = call(&mut rt, "fetch_text", &json!({"url": "download.kde.org/x"}));
    assert_eq!(err_of(&e), "Not a valid URL");

    let e = call(&mut rt, "fetch_text", &json!({"url": "https://evil.example.com/x"}));
    assert_eq!(
        err_of(&e),
        "Host evil.example.com is not on the resolver allowlist"
    );

    // userinfo and port never leak past the host check
    let inst = call(
        &mut rt,
        "fetch_text",
        &json!({"url": "https://user:pass@download.kde.org:443/dir/app.yml"}),
    );
    assert_eq!(inst["ok"], json!(true));
    assert_eq!(inst["need"], json!("net_get"));
    assert_eq!(inst["url"], json!("https://user:pass@download.kde.org:443/dir/app.yml"));

    // a fetched body under the cap passes through untruncated
    let r = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "text"},
                "resp": net_resp(200, "hello world")}),
    );
    assert_eq!(r["result"]["status"], json!(200));
    assert_eq!(r["result"]["body"], json!("hello world"));
    assert_eq!(r["result"]["truncated"], json!(false));

    // content_length above the 2 MiB cap: body NOT read, truncated=true
    let r = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "text"},
                "resp": {"ok": true, "status": 200, "final_url": "https://final",
                         "content_length": 9999999, "body": "", "more": false}}),
    );
    assert_eq!(r["result"]["truncated"], json!(true));
    assert_eq!(r["result"]["body"], json!(""));

    // streaming overflow (no content-length): the host read cap+1, more=true
    let r = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "text"},
                "resp": {"ok": true, "status": 200, "final_url": "https://final",
                         "content_length": null, "body": "partial", "more": true}}),
    );
    assert_eq!(r["result"]["truncated"], json!(true));
    assert_eq!(r["result"]["body"], json!("partial"));
}

// --------------------------------------------------------------- fdroid --

#[test]
fn fdroid_mapping() {
    let mut rt = boot("fdroid");

    let e = call(&mut rt, "fetch_fdroid_package", &json!({"pkg": "  "}));
    assert_eq!(err_of(&e), "No F-Droid package configured");

    let inst = call(&mut rt, "fetch_fdroid_package", &json!({"pkg": "org.app"}));
    assert_eq!(inst["url"], json!("https://f-droid.org/api/v1/packages/org.app"));

    // suggestedVersionCode wins when present among packages
    let body = json!({
        "packages": [
            {"versionCode": 10, "versionName": "1.0"},
            {"versionCode": 12, "versionName": "1.2"}
        ],
        "suggestedVersionCode": 12
    })
    .to_string();
    let r = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "fdroid", "pkg": "org.app"}, "resp": net_resp(200, &body)}),
    );
    assert_eq!(r["result"]["version_code"], json!(12));
    assert_eq!(r["result"]["version"], json!("1.2"));
    assert_eq!(r["result"]["apk_url"], json!("https://f-droid.org/repo/org.app_12.apk"));
    assert_eq!(r["result"]["page_url"], json!("https://f-droid.org/packages/org.app"));

    // a suggested version outside the known set falls back to max_code
    let body = json!({
        "packages": [{"versionCode": 10, "versionName": "1.0"}],
        "suggestedVersionCode": 99
    })
    .to_string();
    let r = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "fdroid", "pkg": "org.app"}, "resp": net_resp(200, &body)}),
    );
    assert_eq!(r["result"]["version_code"], json!(10));

    // no usable versions
    let e = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "fdroid", "pkg": "org.app"},
                "resp": net_resp(200, "{\"packages\":[]}")}),
    );
    assert_eq!(err_of(&e), "No package versions found on F-Droid");

    // 404 and other statuses, byte-exact
    let e = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "fdroid", "pkg": "org.app"}, "resp": net_resp(404, "")}),
    );
    assert_eq!(err_of(&e), "Package not found on F-Droid");
    let e = call(
        &mut rt,
        "fress_on_net",
        &json!({"ctx": {"then": "fdroid", "pkg": "org.app"}, "resp": net_resp(503, "")}),
    );
    assert_eq!(err_of(&e), "F-Droid returned HTTP 503");
}

// ------------------------------------------------------------- sanitize --

#[test]
fn sanitize_filename_cases() {
    let mut rt = boot("sanitize");
    let mut s = |name: &str| -> String {
        let r = call(&mut rt, "sanitize", &json!({"name": name}));
        r["name"].as_str().unwrap().to_string()
    };

    assert_eq!(s("My App (v2).apk"), "My_App_(v2).apk");
    assert_eq!(s("app  setup!.exe"), "app__setup_.exe");
    assert_eq!(s("../../etc/passwd"), ".._.._etc_passwd");
    assert_eq!(s("CON.tar.gz"), "download.bin");
    assert_eq!(s("NUL"), "download.bin");
    assert_eq!(s("com1.txt"), "download.bin");
    assert_eq!(s("lpt4.PDF"), "download.bin");
    assert_eq!(s("COM10.txt"), "COM10.txt", "COM10 is not reserved");
    assert_eq!(s(".."), "download.bin");
    assert_eq!(s("."), "download.bin");
    assert_eq!(s("///"), "download.bin");
    assert_eq!(s("___"), "download.bin");
    assert_eq!(s("日本語.pdf"), "日本語.pdf", "any script's letters survive");
    assert_eq!(s("app🎉.exe"), "app_.exe");
    assert_eq!(s("_leading"), "leading");
}

#[test]
fn percent_decode_cases() {
    let mut rt = boot("pdecode");
    // percent_decode takes a bare string argument (the protocol's
    // JSON-doc fallback passes it through as a raw string).
    let mut d = |s: &str| -> String {
        let out = rt.call("percent_decode", Some(s)).expect("percent_decode");
        serde_json::from_str::<J>(&out)
            .ok()
            .and_then(|v| v.as_str().map(|x| x.to_string()))
            .unwrap_or(out)
    };
    assert_eq!(d("app%20v1.exe"), "app v1.exe");
    assert_eq!(d("%E6%97%A5"), "日");
    assert_eq!(d("%zz"), "%zz", "invalid hex stays literal");
    assert_eq!(d("%2"), "%2", "truncated escape stays literal");
}

// ------------------------------------------------------- start_download --

#[test]
fn start_download_plan_and_ids() {
    let mut rt = boot("start");

    let r = call(
        &mut rt,
        "start_download",
        &json!({"url": "https://x/y.exe", "filename": "my app.exe",
                "directory": "", "expected_sha256": "  ABCD  ",
                "resume": true, "downloads_dir": "/home/u/Downloads"}),
    );
    assert_eq!(r["id"], json!(0));
    assert_eq!(r["plan"]["dir"], json!("/home/u/Downloads"), "empty directory falls back");
    assert_eq!(r["plan"]["name"], json!("my_app.exe"));
    assert_eq!(r["plan"]["expected_sha256"], json!("ABCD"));
    assert_eq!(r["plan"]["resume"], json!(true));

    let r = call(
        &mut rt,
        "start_download",
        &json!({"url": "https://x/z", "filename": null,
                "directory": "/tmp/x", "expected_sha256": null,
                "resume": null, "downloads_dir": "/home/u/Downloads"}),
    );
    assert_eq!(r["id"], json!(1), "ids increment");
    assert_eq!(r["plan"]["dir"], json!("/tmp/x"));
    assert_eq!(r["plan"]["name"], json!(null), "no filename -> worker derives one");
    assert_eq!(r["plan"]["expected_sha256"], json!(""));
    assert_eq!(r["plan"]["resume"], json!(false));

    // cancel: known id flips, unknown stays silent-ok
    let c = call(&mut rt, "cancel_download", &json!({"id": 0}));
    assert_eq!(c["cancel"], json!(true));
    let c = call(&mut rt, "cancel_download", &json!({"id": 999}));
    assert_eq!(c["cancel"], json!(null));
}

// ------------------------------------------------------- retry + verdict --

fn dl_report(id: i64, outcome: &str, extra: J) -> J {
    let mut base = json!({"id": id, "outcome": outcome});
    if let J::Object(map) = extra {
        for (k, v) in map {
            base[k] = v;
        }
    }
    base
}

#[test]
fn retry_policy_network_only() {
    let mut rt = boot("retry");
    call(
        &mut rt,
        "start_download",
        &json!({"url": "https://x/y", "filename": "a.exe", "directory": "/d",
                "expected_sha256": null, "resume": false, "downloads_dir": "/d"}),
    );

    let r = call(&mut rt, "dl_on_result", &dl_report(0, "error", json!({"message": "Connection stalled (no data for 60s)", "kind": "network"})));
    assert_eq!(r["action"], json!("retry"));
    assert_eq!(r["delay_secs"], json!(2));
    assert_eq!(r["payload"]["attempt"], json!(1));
    assert_eq!(r["payload"]["message"], json!("Connection stalled (no data for 60s)"));

    let r = call(&mut rt, "dl_on_result", &dl_report(0, "error", json!({"message": "Download interrupted: reset", "kind": "network"})));
    assert_eq!(r["action"], json!("retry"));
    assert_eq!(r["delay_secs"], json!(4));
    assert_eq!(r["payload"]["attempt"], json!(2));

    // third failure: the policy is exhausted, the error surfaces (with kind)
    let r = call(&mut rt, "dl_on_result", &dl_report(0, "error", json!({"message": "Download interrupted: reset", "kind": "network"})));
    assert_eq!(r["action"], json!("error"));
    assert_eq!(r["payload"]["kind"], json!("network"));

    // a fresh job: non-network kinds are never retried
    call(
        &mut rt,
        "start_download",
        &json!({"url": "https://x/y", "filename": "b.exe", "directory": "/d",
                "expected_sha256": null, "resume": false, "downloads_dir": "/d"}),
    );
    let r = call(&mut rt, "dl_on_result", &dl_report(0, "error", json!({"message": "Server returned HTTP 403", "kind": null})));
    assert_eq!(r["action"], json!("error"));
    assert!(r["payload"].get("kind").is_none() || r["payload"]["kind"] == json!(null));

    // cancelled
    let r = call(&mut rt, "dl_on_result", &dl_report(0, "cancelled", json!({})));
    assert_eq!(r["action"], json!("error"));
    assert_eq!(r["payload"]["message"], json!("Cancelled"));
}

#[test]
fn checksum_verdict() {
    let mut rt = boot("checksum");
    let expected = format!("0x{}", "A".repeat(64));
    call(
        &mut rt,
        "start_download",
        &json!({"url": "https://x/y", "filename": "a.exe", "directory": "/d",
                "expected_sha256": expected, "resume": false,
                "downloads_dir": "/d"}),
    );

    // exact match (case-insensitive, 0x stripped) -> finalize, verified true
    let good = "a".repeat(64);
    let r = call(
        &mut rt,
        "dl_on_result",
        &dl_report(0, "complete", json!({"staging": "/d/a.exe.part", "sha256": good, "bytes": 10})),
    );
    assert_eq!(r["action"], json!("finalize"));
    assert_eq!(r["emit"]["event"], json!("download-complete"));
    assert_eq!(r["emit"]["payload"]["verified"], json!(true));
    assert_eq!(r["emit"]["payload"]["sha256"], json!("a".repeat(64)));

    // mismatch -> delete + the exact original message
    call(
        &mut rt,
        "start_download",
        &json!({"url": "https://x/y", "filename": "b.exe", "directory": "/d",
                "expected_sha256": "A".repeat(64), "resume": false,
                "downloads_dir": "/d"}),
    );
    let r = call(
        &mut rt,
        "dl_on_result",
        &dl_report(1, "complete", json!({"staging": "/d/b.exe.part", "sha256": "b".repeat(64), "bytes": 10})),
    );
    assert_eq!(r["action"], json!("delete_and_error"));
    assert_eq!(r["part"], json!("/d/b.exe.part"));
    let msg = r["payload"]["message"].as_str().unwrap();
    assert!(
        msg.starts_with("Integrity check failed: the file hashes to ")
            && msg.contains(" but the publisher published ")
            && msg.ends_with(". It was deleted. Re-download from the official source."),
        "message: {}",
        msg
    );
    assert_eq!(r["payload"]["kind"], json!("checksum"));

    // no expected hash -> finalize with verified null (never shown as verified)
    call(
        &mut rt,
        "start_download",
        &json!({"url": "https://x/y", "filename": "c.exe", "directory": "/d",
                "expected_sha256": null, "resume": false, "downloads_dir": "/d"}),
    );
    let r = call(
        &mut rt,
        "dl_on_result",
        &dl_report(2, "complete", json!({"staging": "/d/c.exe.part", "sha256": "c".repeat(64), "bytes": 10})),
    );
    assert_eq!(r["action"], json!("finalize"));
    assert_eq!(r["emit"]["payload"]["verified"], json!(null));
}

#[test]
fn unknown_job_reports_plainly() {
    let mut rt = boot("unknown");
    let r = call(&mut rt, "dl_on_result", &dl_report(0, "cancelled", json!({})));
    assert_eq!(r["action"], json!("error"));
    assert_eq!(r["payload"]["message"], json!("Cancelled"));
    let r = call(
        &mut rt,
        "dl_on_result",
        &dl_report(0, "error", json!({"message": "boom", "kind": "network"})),
    );
    assert_eq!(r["action"], json!("error"));
    assert_eq!(r["payload"]["message"], json!("boom"));
}
