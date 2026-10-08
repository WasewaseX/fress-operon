//! build.rs — guarantee the embedded frontend dir exists. CI builds the
//! ORIGINAL Fress frontend (from the pinned upstream checkout) into
//! frontend-dist/ before cargo runs; local dev without the frontend gets
//! a placeholder page instead of a compile failure.

use std::path::PathBuf;
use std::fs;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist = manifest.join("frontend-dist");
    if !dist.exists() {
        fs::create_dir_all(&dist).expect("create frontend-dist");
    }
    let index = dist.join("index.html");
    if !index.exists() {
        fs::write(
            &index,
            "<!doctype html><html><body><div id=\"root\"></div><meta charset=\"utf-8\"><title>Fress Operon</title>\
             <p style=\"font-family:system-ui;padding:2rem\">Frontend not built. \
             Run scripts/build-frontend.sh (CI does this) and rebuild.</p></body></html>",
        )
        .expect("write placeholder");
    }
    println!("cargo:rerun-if-changed=frontend-dist");
    println!("cargo:rerun-if-changed=build.rs");
}
