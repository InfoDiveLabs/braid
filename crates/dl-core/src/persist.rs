//! The transfer list, across a quit.
//!
//! Without this the app forgets what it was doing every time it closes. The
//! files and their journals survive on disk, so the bytes were never at risk,
//! but the rows were: a half-finished download had to be found and added
//! again by hand.
//!
//! Stored as flat `key = value` records separated by blank lines, the same
//! shape and for the same reasons as `settings.conf` next to it.

use crate::engine::{DownloadSpec, Engine, Restorable, State};
use crate::integrity::{Algorithm, Digest};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where the list lives, beside whatever else a front end keeps in `dir`.
///
/// The front end owns the notion of where its configuration lives (a
/// platform data directory, a server's working directory, whatever it is);
/// the engine only knows the name of its own file within it.
fn path(dir: &Path) -> PathBuf {
    dir.join("transfers.conf")
}

/// The state a restored transfer comes back in.
///
/// Running comes back as Queued rather than Running: the engine's concurrency
/// limit decides what actually starts, and a list of twenty that all claimed to
/// be running would start twenty.
fn restored_state(state: State) -> Option<State> {
    match state {
        State::Running | State::Queued => Some(State::Queued),
        State::Paused => Some(State::Paused),
        State::Complete => Some(State::Complete),
        State::Seeding => Some(State::Paused),
        // A failure is not worth restoring: the error it carried is gone, and a
        // row that says "failed" with no reason is worse than no row.
        State::Failed => None,
    }
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Queued => "queued",
        State::Running => "running",
        State::Paused => "paused",
        State::Complete => "complete",
        State::Seeding => "seeding",
        State::Failed => "failed",
    }
}

fn state_from(name: &str) -> Option<State> {
    match name {
        "queued" | "running" => Some(State::Queued),
        "paused" => Some(State::Paused),
        "complete" => Some(State::Complete),
        "seeding" => Some(State::Seeding),
        _ => None,
    }
}

fn encode(entries: &[Restorable]) -> String {
    let mut out = String::from("# Braid transfers. Written by the app.\n");
    for entry in entries {
        out.push_str("\n[transfer]\n");
        out.push_str(&format!("url = {}\n", entry.spec.url));
        out.push_str(&format!("destination = {}\n", entry.spec.destination.display()));
        out.push_str(&format!("connections = {}\n", entry.spec.connections));
        out.push_str(&format!("state = {}\n", state_name(entry.state)));
        out.push_str(&format!("downloaded = {}\n", entry.downloaded));
        if let Some(total) = entry.total {
            out.push_str(&format!("total = {total}\n"));
        }
        if let Some(name) = &entry.name {
            out.push_str(&format!("name = {name}\n"));
        }
        if !entry.spec.interfaces.is_empty() {
            out.push_str(&format!("interfaces = {}\n", entry.spec.interfaces.join(",")));
        }
        if let Some(expect) = &entry.spec.expect {
            out.push_str(&format!(
                "expect = {}:{}\n",
                expect.algorithm().as_str(),
                expect.to_hex()
            ));
        }
        // A `BTreeMap` rather than the insertion order a front end used,
        // so the file a person opens by hand is stable across a save that
        // touched none of the labels.
        for (key, value) in &entry.labels {
            out.push_str(&format!("label.{key} = {value}\n"));
        }
    }
    out
}

fn decode(text: &str) -> Vec<Restorable> {
    let mut entries = Vec::new();
    let mut record = Record::default();

    for line in text.lines() {
        let line = line.trim();
        if line == "[transfer]" {
            record.take().flush(&mut entries);
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "url" => record.url = Some(value.to_string()),
            "destination" => record.destination = Some(PathBuf::from(value)),
            "connections" => record.connections = value.parse().unwrap_or(DEFAULT_CONNECTIONS),
            "state" => record.state = state_from(value).unwrap_or(State::Queued),
            "downloaded" => record.downloaded = value.parse().unwrap_or(0),
            "total" => record.total = value.parse().ok(),
            "name" => record.name = Some(value.to_string()),
            "interfaces" => {
                record.interfaces = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
            }
            "expect" => {
                record.expect = value
                    .split_once(':')
                    .and_then(|(algorithm, hex)| Digest::parse(Algorithm::parse(algorithm)?, hex));
            }
            // Kept for any key under `label.`, known to this build or not.
            // A label is a front end's own compatibility detail, not a
            // vocabulary this crate agrees to: a server upgraded to keep a
            // new one and then downgraded must not have this file quietly
            // erase it the next time an older build rewrites the file.
            _ if key.starts_with("label.") => {
                record.labels.insert(key["label.".len()..].to_string(), value.to_string());
            }
            _ => {}
        }
    }
    record.flush(&mut entries);
    entries
}

const DEFAULT_CONNECTIONS: usize = 8;

/// One record as it is being read, before it is known to be complete.
struct Record {
    url: Option<String>,
    destination: Option<PathBuf>,
    connections: usize,
    state: State,
    downloaded: u64,
    total: Option<u64>,
    name: Option<String>,
    interfaces: Vec<String>,
    expect: Option<Digest>,
    labels: BTreeMap<String, String>,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            url: None,
            destination: None,
            connections: DEFAULT_CONNECTIONS,
            state: State::Queued,
            downloaded: 0,
            total: None,
            name: None,
            interfaces: Vec::new(),
            expect: None,
            labels: BTreeMap::new(),
        }
    }
}

impl Record {
    fn take(&mut self) -> Self {
        std::mem::take(self)
    }

    /// A record missing a url or a destination is dropped whole rather than
    /// guessed at: half a transfer would download to somewhere nobody asked
    /// for.
    fn flush(self, entries: &mut Vec<Restorable>) {
        let (Some(url), Some(destination)) = (self.url, self.destination) else { return };
        let mut spec = DownloadSpec::new(url, destination);
        spec.connections = self.connections.max(1);
        spec.interfaces = self.interfaces;
        spec.expect = self.expect;
        entries.push(Restorable {
            spec,
            state: self.state,
            downloaded: self.downloaded,
            total: self.total,
            name: self.name,
            labels: self.labels,
        });
    }
}

/// The list as it would be written right now.
fn current(engine: &Engine) -> String {
    let entries: Vec<Restorable> =
        engine.specs().into_iter().filter(|e| restored_state(e.state).is_some()).collect();
    encode(&entries)
}

/// Write the list, through a temporary file so an interrupted save leaves the
/// previous one rather than half of this one.
fn write(dir: &Path, text: &str) {
    let path = path(dir);
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let temp = path.with_extension("conf.tmp");
    if std::fs::write(&temp, text).is_ok() {
        let _ = std::fs::rename(&temp, &path);
    }
}

/// Keep the file in step with the engine.
///
/// A poll rather than a hook on every callsite: a transfer changes state from
/// half a dozen places (the add sheet, the tray, a magnet from the browser, the
/// scheduler, a download simply finishing), and one of them would eventually be
/// missed. Comparing the encoded text means an idle app writes nothing.
pub fn spawn_autosave(engine: Engine, dir: PathBuf) {
    std::thread::spawn(move || {
        let mut written = current(&engine);
        write(&dir, &written);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let now = current(&engine);
            if now != written {
                write(&dir, &now);
                written = now;
            }
        }
    });
}

/// Put back what the last run was doing. Returns how many rows came back.
pub fn restore(engine: &Engine, dir: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(path(dir)) else { return 0 };

    let mut restored = 0;
    for mut entry in decode(&text) {
        let Some(state) = restored_state(entry.state) else { continue };
        entry.state = state;
        engine.restore(entry);
        restored += 1;
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str, state: State) -> Restorable {
        Restorable {
            spec: DownloadSpec::new(url, "/tmp/x.iso"),
            state,
            downloaded: 0,
            total: None,
            name: None,
            labels: BTreeMap::new(),
        }
    }

    /// A record for tests that do not care about its state.
    fn record_for(url: &str) -> Restorable {
        entry(url, State::Queued)
    }

    #[test]
    fn a_list_survives_a_round_trip() {
        let mut one = entry("https://example.test/a.iso", State::Paused);
        one.spec.connections = 12;
        one.spec.interfaces = vec!["en0".into(), "en1".into()];
        one.downloaded = 4096;
        one.total = Some(8192);
        let two = entry("magnet:?xt=urn:btih:ab", State::Complete);

        let back = decode(&encode(&[one, two]));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].spec.url, "https://example.test/a.iso");
        assert_eq!(back[0].spec.connections, 12);
        assert_eq!(back[0].spec.interfaces, vec!["en0".to_string(), "en1".to_string()]);
        assert_eq!(back[0].state, State::Paused);
        assert_eq!((back[0].downloaded, back[0].total), (4096, Some(8192)));
        assert_eq!(back[1].spec.url, "magnet:?xt=urn:btih:ab");
    }

    #[test]
    fn a_torrent_keeps_the_name_it_learned() {
        // Otherwise a restored torrent reads as its magnet URI, which is what
        // the row looked like before its metadata ever arrived.
        let mut one = entry("magnet:?xt=urn:btih:ab", State::Paused);
        one.name = Some("debian-13.iso".into());
        let back = decode(&encode(&[one]));
        assert_eq!(back[0].name.as_deref(), Some("debian-13.iso"));
    }

    #[test]
    fn a_paused_transfer_does_not_resume_itself() {
        // Coming back Running would be the opposite of a pause.
        assert_eq!(restored_state(State::Paused), Some(State::Paused));
    }

    #[test]
    fn a_running_transfer_comes_back_queued() {
        // The concurrency limit decides what starts, not the file: twenty rows
        // that all claimed to be running would start twenty.
        assert_eq!(restored_state(State::Running), Some(State::Queued));
        assert_eq!(restored_state(State::Queued), Some(State::Queued));
    }

    #[test]
    fn a_failure_is_not_restored() {
        // The error it carried is gone, and a row saying "failed" with no
        // reason is worse than no row.
        assert_eq!(restored_state(State::Failed), None);
    }

    #[test]
    fn a_record_missing_its_destination_is_dropped_whole() {
        // Half a transfer would download somewhere nobody asked for.
        assert!(decode("[transfer]\nurl = https://example.test/a\n").is_empty());
    }

    #[test]
    fn a_digest_survives_the_file() {
        let mut one = entry("https://example.test/a.iso", State::Queued);
        one.spec.expect = Digest::parse(Algorithm::Sha256, &"ab".repeat(32));
        let back = decode(&encode(&[one]));
        assert_eq!(back[0].spec.expect.as_ref().map(|d| d.algorithm()), Some(Algorithm::Sha256));
    }

    #[test]
    fn a_corrupt_file_costs_the_records_it_could_not_read_and_no_more() {
        let back = decode("garbage\n[transfer]\nurl = https://e.test/a\ndestination = /tmp/a\n");
        assert_eq!(back.len(), 1);
    }

    #[test]
    fn labels_survive_a_restart_and_the_engine_never_reads_them() {
        // The server keeps a category, a pair of timestamps and whatever else a
        // front end needs here. The engine stores them and has no opinion: a
        // category is a qBittorrent idea, not a download one, and teaching the
        // engine about it would put a compatibility detail in the wrong crate.
        let mut record = record_for("https://example.test/x.iso");
        record.labels.insert("category".into(), "tv-sonarr".into());
        record.labels.insert("added_on".into(), "1790000000".into());

        let back = decode(&encode(&[record]));
        assert_eq!(back[0].labels.get("category").map(String::as_str), Some("tv-sonarr"));
        assert_eq!(back[0].labels.get("added_on").map(String::as_str), Some("1790000000"));
    }

    #[test]
    fn a_label_with_an_equals_sign_in_it_round_trips() {
        // The file is `key = value` lines, so a value containing the separator is
        // the case that corrupts the record after it.
        let mut record = record_for("https://example.test/x.iso");
        record.labels.insert("note".into(), "a=b".into());
        assert_eq!(decode(&encode(&[record]))[0].labels.get("note").unwrap(), "a=b");
    }
}
