//! Translating Braid's own transfer states into qBittorrent's vocabulary.
//!
//! This is its own file, not a method tucked onto `DownloadSnapshot` or a
//! branch inline in a handler, because a wrong mapping here is silent. Sonarr
//! and Radarr decide whether a download has finished by reading the `state`
//! string this module produces, not by reading `progress`. Get it wrong and
//! the client fetches every byte correctly and never hands the file over, and
//! nothing anywhere logs an error: the download simply sits in the wrong
//! bucket forever. The strings this module returns are qBittorrent's spelling
//! of its own states, not ours: they are what somebody else's parser expects
//! to find, and are not free to be renamed or tidied to taste.

//!
//! Written before its caller, and allowed to be, which is the point. It was
//! built and tested on its own precisely because it is the risky part, ahead
//! of the listing endpoint that will be its only caller. Lift the allow below
//! when that lands.

#![allow(dead_code)]

pub(crate) mod app;
mod state;
