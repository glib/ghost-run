//! Bakes the app's version in as `GHOST_RUN_VERSION` (`docs/releasing.md`).
//! Releases set it in CI from the tag they make. Other builds take it from
//! `git describe`: `1.2.3` on a release's own commit, `1.2.3+4.gabc1234`
//! four commits later (`.dirty` with uncommitted changes), which orders the
//! same as 1.2.3, so the next release is still offered. No tag or no git:
//! `0.0.0+dev`.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GHOST_RUN_VERSION");
    let version = match std::env::var("GHOST_RUN_VERSION") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_start_matches('v').to_owned(),
        _ => {
            watch_git();
            git(&[
                "describe", "--tags", "--match", "v[0-9]*", "--long", "--dirty",
            ])
            .and_then(|d| from_describe(&d))
            .unwrap_or_else(|| "0.0.0+dev".into())
        }
    };
    println!("cargo:rustc-env=GHOST_RUN_VERSION={version}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Rebuilds when HEAD moves or a tag is made.
fn watch_git() {
    for (flag, files) in [
        ("--git-dir", &["HEAD", "index"][..]),
        ("--git-common-dir", &["refs/tags", "packed-refs"][..]),
    ] {
        if let Some(dir) = git(&["rev-parse", flag]) {
            for f in files {
                println!("cargo:rerun-if-changed={dir}/{f}");
            }
        }
    }
}

/// `v1.2.3-0-gabc` → `1.2.3`; `v1.2.3-4-gabc-dirty` → `1.2.3+4.gabc.dirty`.
fn from_describe(describe: &str) -> Option<String> {
    let (rest, dirty) = match describe.strip_suffix("-dirty") {
        Some(r) => (r, true),
        None => (describe, false),
    };
    let mut parts = rest.rsplitn(3, '-');
    let hash = parts.next()?;
    let count: u32 = parts.next()?.parse().ok()?;
    let tag = parts.next()?.trim_start_matches('v');
    let mut meta = Vec::new();
    if count > 0 {
        meta.push(count.to_string());
        meta.push(hash.to_owned());
    }
    if dirty {
        meta.push("dirty".to_owned());
    }
    Some(if meta.is_empty() {
        tag.to_owned()
    } else {
        format!("{tag}+{}", meta.join("."))
    })
}
