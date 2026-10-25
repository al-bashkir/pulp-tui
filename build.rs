//! Sets `PULP_TUI_VERSION`: the package version, plus `git describe` when built from a checkout.

use std::path::Path;
use std::process::Command;

fn main() {
    // Only existing paths: a missing one makes cargo rerun this (and rebuild the crate) every time.
    for p in ["src", ".git/HEAD", ".git/index", ".git/refs", ".git/packed-refs"] {
        if Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    let pkg = env!("CARGO_PKG_VERSION");
    let git = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|g| !g.is_empty() && *g != format!("v{pkg}"));
    let v = match git {
        Some(g) => format!("v{pkg} ({g})"),
        None => format!("v{pkg}"),
    };
    println!("cargo:rustc-env=PULP_TUI_VERSION={v}");
}
