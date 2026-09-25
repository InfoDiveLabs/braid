use dl_core::{DownloadSnapshot, Progress, State};

/// qBittorrent's spelling of "I do not know how long this will take".
///
/// Zero means "finished now" to a client reading this field, which is the
/// worst possible answer to "I do not know": it would read as though the
/// transfer had already arrived rather than as an absent estimate.
const UNKNOWN_ETA_SECONDS: u64 = 8_640_000;

/// The qBittorrent `state` string for one transfer.
///
/// Sonarr and Radarr key their "is this done" decision off exactly this
/// string, not off `progress`, which is the whole reason this function
/// exists rather than folding a couple of `if`s into whatever assembles the
/// API response.
pub fn qbit_state(snapshot: &DownloadSnapshot) -> &'static str {
    let complete =
        snapshot.progress.total.is_some_and(|total| snapshot.progress.downloaded >= total);

    match snapshot.state {
        State::Queued => "queuedDL",
        State::Running => {
            // A resume re-validating what is already on disk is not moving
            // new bytes and must not read as stalled: it is busy, just not at
            // the thing a byte counter would show.
            if snapshot.phase.as_deref() == Some("Checking") {
                "checkingDL"
            } else if snapshot.progress.bytes_per_sec > 0 {
                "downloading"
            } else {
                "stalledDL"
            }
        }
        // A paused transfer that has every byte is done, not abandoned:
        // Sonarr reads `pausedUP` as finished and `pausedDL` as abandoned,
        // and getting this backwards strands every completed download.
        State::Paused => {
            if complete {
                "pausedUP"
            } else {
                "pausedDL"
            }
        }
        // Reaching `State::Complete` at all already means every byte is on
        // disk, so there is nothing left for `complete` to gate here.
        State::Complete => "pausedUP",
        // Seeding splits on whether bytes are actually leaving, not on
        // whether the state is seeding at all: `stalledUP` tells a client
        // there is nothing to wait for, which is false the moment upload
        // traffic is flowing.
        State::Seeding => {
            let uploading =
                snapshot.torrent.as_ref().is_some_and(|torrent| torrent.upload_bytes_per_sec > 0);
            if uploading { "uploading" } else { "stalledUP" }
        }
        State::Failed => "error",
    }
}

/// Seconds until finished, in qBittorrent's own units, from the smoothed
/// rate rather than the instantaneous one.
///
/// The instantaneous rate swings hard enough on its own (see the comment on
/// [`Progress::smoothed_bytes_per_sec`]) that an estimate built from it jumps
/// around every tick, which reads as broken in exactly the place a client is
/// showing it as a single settled number. Unknown is reported as
/// [`UNKNOWN_ETA_SECONDS`], qBittorrent's own spelling of infinity, rather
/// than zero: zero reads as "arriving now".
pub fn eta_seconds(progress: &Progress) -> u64 {
    let Some(total) = progress.total else { return UNKNOWN_ETA_SECONDS };
    let Some(remaining) = total.checked_sub(progress.downloaded) else {
        return UNKNOWN_ETA_SECONDS;
    };
    if progress.smoothed_bytes_per_sec == 0 {
        return UNKNOWN_ETA_SECONDS;
    }
    remaining / progress.smoothed_bytes_per_sec
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_core::engine::DownloadId;
    use dl_core::torrent::TorrentStatus;

    fn snap(
        state: State,
        downloaded: u64,
        total: Option<u64>,
        rate: u64,
        phase: Option<&str>,
    ) -> DownloadSnapshot {
        DownloadSnapshot {
            id: DownloadId(0),
            filename: "file.bin".into(),
            host: "example.test".into(),
            state,
            progress: Progress {
                downloaded,
                total,
                bytes_per_sec: rate,
                smoothed_bytes_per_sec: rate,
            },
            lanes: Vec::new(),
            phase: phase.map(str::to_string),
            error: None,
            torrent: None,
        }
    }

    // A named type alias for a tuple used in exactly this one table would be
    // one more name to look up for no clarity gained over reading the table
    // itself.
    #[allow(clippy::type_complexity)]
    #[test]
    fn every_braid_state_has_a_qbittorrent_spelling() {
        let cases: &[(State, u64, Option<u64>, u64, Option<&str>, &str)] = &[
            (State::Queued, 0, Some(100), 0, None, "queuedDL"),
            (State::Running, 10, Some(100), 5, None, "downloading"),
            (State::Running, 10, Some(100), 0, None, "stalledDL"),
            (State::Running, 10, Some(100), 0, Some("Checking"), "checkingDL"),
            (State::Paused, 10, Some(100), 0, None, "pausedDL"),
            (State::Paused, 100, Some(100), 0, None, "pausedUP"),
            (State::Complete, 100, Some(100), 0, None, "pausedUP"),
            (State::Seeding, 100, Some(100), 0, None, "stalledUP"),
            (State::Failed, 10, Some(100), 0, None, "error"),
        ];
        for (state, done, total, rate, phase, want) in cases {
            let got = qbit_state(&snap(*state, *done, *total, *rate, *phase));
            assert_eq!(got, *want, "{state:?} done={done} rate={rate} phase={phase:?}");
        }
    }

    #[test]
    fn a_seeding_torrent_that_is_uploading_is_not_stalled() {
        // `stalledUP` tells a client there is nothing to wait for. A torrent
        // that is actively giving bytes away is exactly what somebody
        // watching a ratio is waiting for.
        let mut s = snap(State::Seeding, 100, Some(100), 0, None);
        s.torrent = Some(TorrentStatus { upload_bytes_per_sec: 1_000_000, ..Default::default() });
        assert_eq!(qbit_state(&s), "uploading");
    }

    #[test]
    fn a_paused_transfer_that_finished_reports_as_paused_upload_not_download() {
        // Sonarr reads `pausedUP` as done and `pausedDL` as abandoned.
        // Getting this backwards silently strands every completed download.
        assert_eq!(qbit_state(&snap(State::Paused, 100, Some(100), 0, None)), "pausedUP");
    }

    #[test]
    fn an_unknown_length_never_reports_as_complete() {
        // `downloaded >= total` cannot be evaluated without a total, and
        // guessing yes would mark a running transfer finished.
        assert_eq!(qbit_state(&snap(State::Running, 500, None, 10, None)), "downloading");
        assert_eq!(qbit_state(&snap(State::Paused, 500, None, 0, None)), "pausedDL");
    }

    #[test]
    fn an_unknown_estimate_is_qbittorrents_infinity_rather_than_zero() {
        // Zero means "finished now" to a client, and is the worst possible
        // answer to "I do not know".
        assert_eq!(eta_seconds(&Progress { total: None, ..Default::default() }), 8_640_000);
        assert_eq!(
            eta_seconds(&Progress {
                downloaded: 0,
                total: Some(100),
                smoothed_bytes_per_sec: 0,
                ..Default::default()
            }),
            8_640_000
        );
    }
}
