//! Build stamp for the `System > Debug` screen.
//!
//! Embeds the commit the UI binary was built from, the build time and the profile, so the
//! screen can say definitively which software is on the device. A `-dirty` suffix means the
//! working tree had uncommitted changes at build time (the hash alone would then be a lie).
//!
//! Shells out to `git` and `date`; there is no build-dependency, and both are present on the
//! machine that cross-compiles for the CM4. Any failure degrades to `unknown` rather than
//! failing the build.

use std::process::Command;

fn out(key: &str, value: &str) {
    println!("cargo:rustc-env={key}={value}");
}

fn stdout(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    // Cargo's default "rerun when a file in the package changes" is **not** enough here: a
    // commit that touches only another crate (`bins/plippa`, say) changes the stamp's value
    // but no file in this package, so the build script would not re-run and the screen would
    // keep reporting the previous commit. Emitting any `rerun-if-changed` replaces that
    // default, so the package's own sources are listed explicitly alongside the git metadata.
    //
    // `.git/HEAD` moves on a commit or checkout (the commit hash must always refresh);
    // `.git/index` moves on `git add` (the `-dirty` marker). An unstaged edit to a file
    // outside this package is not watched — the dirty flag can lag until the next rebuild of
    // this crate, which is why the hash, not the suffix, is the authoritative field.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");
    if let Some(dir) = stdout(Command::new("git").args(["rev-parse", "--git-dir"])) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
    }

    let git = stdout(Command::new("git").args(["rev-parse", "--short=7", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|o| o.status.success() && !o.stdout.is_empty());

    let time = stdout(Command::new("date").args(["-u", "+%Y-%m-%d %H:%M"]))
        .unwrap_or_else(|| "unknown".to_string());

    out(
        "PLIPPA_BUILD_GIT",
        &format!("{git}{}", if dirty { "-dirty" } else { "" }),
    );
    out("PLIPPA_BUILD_TIME", &time);
    out(
        "PLIPPA_BUILD_PROFILE",
        &std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into()),
    );
}
