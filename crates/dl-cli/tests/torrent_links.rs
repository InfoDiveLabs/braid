//! What `dl add` does with a torrent link, through the real binary.
//!
//! Both cases here are about the message, not the transfer. A magnet link that
//! fails is the one failure a user cannot debug from the outside: the link is
//! opaque, so an error that does not name the actual reason sends them looking
//! for a typo in forty characters of hex.

use std::process::Stdio;

fn dl() -> &'static str {
    env!("CARGO_BIN_EXE_dl")
}

#[cfg(not(feature = "torrent"))]
const MAGNET: &str = "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862&dn=Thing";

async fn add(link: &str) -> String {
    let output = tokio::process::Command::new(dl())
        .args(["add", link])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("dl runs");
    assert!(!output.status.success(), "dl add {link} was expected to fail");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A magnet link with no info hash names nothing. The message has to say which
/// part is missing, because "invalid url" points at the wrong thing entirely.
#[tokio::test(flavor = "multi_thread")]
async fn an_incomplete_magnet_link_names_the_missing_info_hash() {
    let message = add("magnet:?dn=something").await;
    assert!(message.contains("xt=urn:btih:"), "{message}");
}

/// In a build without the feature, a magnet must fail on the feature rather
/// than fall through to the HTTP path: which would report an unparseable URL
/// for a link that is perfectly well formed.
#[cfg(not(feature = "torrent"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_build_without_torrent_support_says_so_and_names_the_feature() {
    let message = add(MAGNET).await;
    assert!(message.contains("torrent support"), "{message}");
    assert!(message.contains("--features torrent"), "{message}");
    assert!(!message.contains("invalid url"), "it must not read as a broken link: {message}");
}

/// With the feature on, a `.torrent` path is read as a file rather than
/// fetched as a URL. Pointing it at one that is not there is the cheapest way
/// to prove which path it took: the failure names the file, where the HTTP
/// path would have complained about the URL.
#[cfg(feature = "torrent")]
#[tokio::test(flavor = "multi_thread")]
async fn a_local_torrent_path_is_read_from_disk_not_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nothing-here.torrent");
    let message = add(&missing.to_string_lossy()).await;

    assert!(message.contains("nothing-here.torrent"), "{message}");
    assert!(!message.contains("invalid url"), "it was sent down the HTTP path: {message}");
}
