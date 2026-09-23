//! `dl add --refresh-command` and `--mirror`, through the real binary.
//!
//! Unix only: the test writes an executable script to stand in for the
//! resolver, which is what `yt-dlp -g` is in practice.
#![cfg(unix)]

use dl_testkit::{Origin, Scenario, fixtures, scenario::SEED};
use std::process::Stdio;

fn dl() -> &'static str {
    env!("CARGO_BIN_EXE_dl")
}

/// A resolver that prints the JSON a refresher expects.
fn resolver(dir: &std::path::Path, reply: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("resolve.sh");
    // The URL arrives as `$1`; ignoring it is what a resolver that mints a new
    // link from its own state does.
    std::fs::write(&path, format!("#!/bin/sh\ncat <<'REPLY'\n{reply}\nREPLY\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_link_is_replaced_by_the_refresh_command() {
    let size = 512 << 10;
    let origin = Origin::spawn(Scenario::Returns410Gone { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("payload.bin");

    // Minted before the download starts, exactly as an out-of-band resolver
    // would hold a credential of its own.
    let fresh = origin.sign("payload.bin", None);
    let headers =
        fresh.headers.iter().map(|(k, v)| format!("\"{k}\":\"{v}\"")).collect::<Vec<_>>().join(",");
    let script =
        resolver(dir.path(), &format!("{{\"url\":\"{}\",\"headers\":{{{headers}}}}}", fresh.url));

    let status = tokio::process::Command::new(dl())
        .args(["add", &origin.url("payload.bin"), "-o"])
        .arg(&dest)
        .arg("--refresh-command")
        .arg(&script)
        .args(["--chunk-size", "65536", "-n", "4"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .unwrap();

    assert!(status.success(), "the download should have recovered from 410");
    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
    assert!(origin.rejections() > 0, "the withdrawn link was never actually served");
}

#[tokio::test(flavor = "multi_thread")]
async fn mirrors_named_on_the_command_line_become_lanes() {
    let size = 2 << 20;
    let a = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let b = Origin::spawn(Scenario::Ok200 { size }).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("payload.bin");

    let status = tokio::process::Command::new(dl())
        .args(["add", &a.url("payload.bin"), "-o"])
        .arg(&dest)
        .args(["--mirror", &b.url("payload.bin")])
        .args(["--chunk-size", "65536", "-n", "6"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .unwrap();

    assert!(status.success());
    assert_eq!(blake3::hash(&std::fs::read(&dest).unwrap()), fixtures::digest(SEED, size));
    assert!(b.requests() > 0, "the mirror was accepted but never used");
}
