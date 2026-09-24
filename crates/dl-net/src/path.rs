//! Every way a lane can reach the origin, in one list.
//!
//! The engine asks for a `LaneSet` and weighs whatever it is given. It does
//! not care whether a lane is bound to a local card or points at a phone, so
//! there is no reason for those to be different types: a transfer using
//! Ethernet, Wi-Fi and two phones at once is the entire point of this project,
//! and that is only expressible if they share one list.
//!
//! One flat `Vec` rather than a set of sets. Nesting them would mean an outer
//! lane index and an inner one, and mapping between two index spaces is the
//! mistake that already cost this project a bug in the inspector's tabs.

use crate::http::{HttpConfig, HttpSource, ProxyMode};
use crate::iface::InterfaceProvider;
use crate::multi::InterfaceLanes;
use crate::relay::Relay;
use dl_core::error::{Error, Result};
use dl_core::lane::{Joined, LaneSet};
use dl_core::source::ByteSource;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How one lane reaches the origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Path {
    /// Whatever the routing table decides, carrying the name of the interface
    /// it will decide on.
    ///
    /// The name is not decoration. Without it this lane is labelled "default
    /// route" and appears in the sidebar as a card that does not exist,
    /// carrying all the traffic while the interface actually doing the work
    /// sits at zero beside it.
    Default(String),
    /// Bound to a local interface by name.
    Interface(String),
    /// Through a relay, over one of the networks it offers.
    Relay { relay: Relay, network: String },
}

impl Path {
    /// What the sidebar shows.
    ///
    /// The relay case names the device and which of its networks this is: a
    /// phone contributing two lanes would otherwise show one label twice, and
    /// the whole reason to show throughput per lane is to tell them apart.
    pub fn label(&self, resolved: Option<&str>) -> String {
        match self {
            Self::Default(name) => resolved.unwrap_or(name).to_string(),
            Self::Interface(name) => resolved.unwrap_or(name).to_string(),
            Self::Relay { relay, network } => format!("{} ({network})", relay.name),
        }
    }
}

/// The most lanes one transfer will open, counting replacements.
///
/// A path that comes and goes gets a new lane each time it comes back, and
/// without a ceiling a phone flapping on a weak signal would add one for the
/// length of the transfer. Generous enough that no honest setup reaches it.
const MAX_LANES: usize = 32;

/// How long to leave a path alone after replacing its lane.
///
/// A phone that is simply off keeps being offered, because the desktop
/// remembers what it last said it had; without a wait, every time its
/// replacement lane failed its chunks and parked, another would be opened.
/// This turns that into one attempt a quarter of a minute, which is also about
/// the right cadence for a phone that has gone out of range.
const REOPEN_COOLDOWN: Duration = Duration::from_secs(15);

/// Asked, while a transfer is running, which paths the app would use now.
///
/// A phone paired thirty seconds into a six gigabyte download is no use if the
/// transfer captured its lane list at the start; this is how the answer is
/// allowed to change.
pub type Paths = Arc<dyn Fn() -> Vec<Path> + Send + Sync>;

/// Everything needed to open a lane for a path that turns up later.
struct Watch {
    paths: Paths,
    url: String,
    config: HttpConfig,
    provider: Arc<dyn InterfaceProvider>,
    /// The paths already considered, in lane order, so a path that is already
    /// carrying a lane is not opened twice.
    taken: Mutex<Vec<Opened>>,
}

/// One path this set has turned into a lane, and when.
struct Opened {
    path: Path,
    /// When this lane replaced a dead one. `None` for the lanes the transfer
    /// began with, which are never something to wait before retrying.
    replaced_at: Option<Instant>,
}

/// Open one lane for one path.
///
/// Shared by the initial build and by anything that joins later, so a phone
/// that arrives mid-transfer is opened exactly the way it would have been had
/// it been there from the start.
fn open(
    path: &Path,
    url: &str,
    config: &HttpConfig,
    provider: &dyn InterfaceProvider,
) -> Result<(String, HttpSource)> {
    match path {
        Path::Default(_) => Ok((path.label(None), HttpSource::with_config(config, url)?)),
        Path::Interface(name) => {
            // Reuses the binding matrix rather than repeating it. That code is
            // the least portable part of this project and the only part that
            // cannot be checked on the machine it was written on, so it should
            // exist exactly once.
            let bound =
                InterfaceLanes::from_names(provider, std::slice::from_ref(name), url, config)?;
            let lane = bound
                .into_lanes()
                .pop()
                .ok_or_else(|| Error::Transport(format!("no usable interface named {name:?}")))?;
            // The binding actually achieved, not the one asked for: a silent
            // fall back to a plain source-address bind has to be visible rather
            // than assumed.
            let label = path.label(Some(&lane.label()));
            Ok((label, lane.into_source()))
        }
        Path::Relay { relay, network } => {
            let config =
                HttpConfig { proxy: ProxyMode::Manual(relay.proxy_url(network)), ..config.clone() };
            Ok((path.label(None), HttpSource::with_config(&config, url)?))
        }
    }
}

/// The lanes a transfer will use, one per path.
pub struct PathLanes {
    sources: Vec<HttpSource>,
    labels: Vec<String>,
    /// Kept so a lane can be opened later on the same terms as these were.
    url: String,
    config: HttpConfig,
    initial: Vec<Path>,
    watch: Option<Watch>,
}

impl PathLanes {
    /// Build one lane per path.
    ///
    /// Nothing is contacted here. A relay that is asleep, or an interface that
    /// just went down, becomes a lane that fails its first request and is
    /// parked, which costs one request. Probing up front would cost every
    /// transfer a round trip to every path before it could start, and a phone
    /// that woke a second late would lose a path it could have served.
    pub fn build(
        paths: &[Path],
        url: &str,
        config: &HttpConfig,
        provider: &dyn InterfaceProvider,
    ) -> Result<Self> {
        if paths.is_empty() {
            return Err(Error::Transport("no paths to the origin".into()));
        }

        let mut sources = Vec::with_capacity(paths.len());
        let mut labels = Vec::with_capacity(paths.len());

        for path in paths {
            let (label, source) = open(path, url, config, provider)?;
            labels.push(label);
            sources.push(source);
        }

        Ok(Self {
            sources,
            labels,
            url: url.to_string(),
            config: config.clone(),
            initial: paths.to_vec(),
            watch: None,
        })
    }

    /// Keep watching `paths` for the life of the transfer.
    ///
    /// Without this a transfer's lanes are whatever existed when it started,
    /// which is the difference between pairing a phone and pairing a phone
    /// that does something. Anything `paths` reports and this set does not
    /// already carry becomes a new lane; anything it stops reporting is left
    /// alone, because a lane that has gone away fails its next chunk and is
    /// parked, and taking it out from under the chunk in flight would not.
    pub fn watching(mut self, paths: Paths, provider: Arc<dyn InterfaceProvider>) -> Self {
        let url = self.url.clone();
        let config = self.config.clone();
        let taken = Mutex::new(
            self.initial
                .iter()
                .map(|path| Opened { path: path.clone(), replaced_at: None })
                .collect(),
        );
        self.watch = Some(Watch { paths, url, config, provider, taken });
        self
    }
}

impl LaneSet for PathLanes {
    fn len(&self) -> usize {
        self.sources.len()
    }

    fn source(&self, lane: usize) -> &dyn ByteSource {
        &self.sources[lane]
    }

    fn label(&self, lane: usize) -> &str {
        &self.labels[lane]
    }

    fn joined(&self, live: &[bool]) -> Vec<Joined> {
        let Some(watch) = &self.watch else { return Vec::new() };
        let mut taken = watch.taken.lock().unwrap();
        let mut fresh = Vec::new();

        for path in (watch.paths)() {
            // `taken` is in lane order, so a path is already served if any lane
            // built from it is still in rotation. Checking only whether the
            // path is familiar was the bug: a phone whose owner switched
            // sharing off left a parked lane behind, and switching it back on
            // did nothing at all for the rest of the transfer, because the path
            // had been seen before.
            let served = taken
                .iter()
                .enumerate()
                .any(|(lane, seen)| seen.path == path && live.get(lane).copied().unwrap_or(true));
            if served {
                continue;
            }
            // Recently replaced, and the replacement is already gone. Waiting
            // is the only useful thing left to do.
            if taken.iter().any(|seen| {
                seen.path == path
                    && seen.replaced_at.is_some_and(|at| at.elapsed() < REOPEN_COOLDOWN)
            }) {
                continue;
            }
            if taken.len() >= MAX_LANES {
                tracing::warn!(
                    limit = MAX_LANES,
                    "not opening another lane; a path is coming and going faster than it is useful"
                );
                break;
            }
            // Recorded whether or not it opens. A path that cannot be opened
            // now is not going to open on the next tick either, and retrying it
            // forever would fill the log rather than the file. It is still
            // eligible to come back later: a failed open leaves no live lane,
            // so the check above will offer it again.
            taken.push(Opened { path: path.clone(), replaced_at: Some(Instant::now()) });
            match open(&path, &watch.url, &watch.config, watch.provider.as_ref()) {
                Ok((label, source)) => fresh.push(Joined { label, source: Arc::new(source) }),
                Err(e) => tracing::warn!(
                    error = %e,
                    "a path that appeared mid-transfer could not be opened"
                ),
            }
        }
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iface::FakeInterfaces;

    fn relay() -> Relay {
        Relay::new("Pixel", "127.0.0.1:1", Some("key".into()))
    }

    fn build(paths: &[Path]) -> Result<PathLanes> {
        PathLanes::build(
            paths,
            "http://example.test/x",
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
    }

    /// Build a watching set whose idea of "what paths exist now" is a slot a
    /// test can change, the way pairing a phone or switching sharing off does.
    fn watching(initial: &[Path], now: Arc<Mutex<Vec<Path>>>) -> PathLanes {
        let live = Arc::clone(&now);
        build(initial).unwrap().watching(
            Arc::new(move || live.lock().unwrap().clone()),
            Arc::new(FakeInterfaces(Vec::new())),
        )
    }

    #[test]
    fn a_path_that_appears_mid_transfer_becomes_a_lane() {
        let local = Path::Default("en0".into());
        let phone = Path::Relay { relay: relay(), network: "cell".into() };
        let now = Arc::new(Mutex::new(vec![local.clone()]));
        let lanes = watching(std::slice::from_ref(&local), Arc::clone(&now));

        assert!(lanes.joined(&[true]).is_empty(), "nothing has changed yet");
        now.lock().unwrap().push(phone);
        let fresh = lanes.joined(&[true]);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].label, "Pixel (cell)");
    }

    #[test]
    fn a_phone_that_comes_back_gets_a_new_lane() {
        // The bug this exists for: switching sharing off parked the phone's
        // lane, and switching it back on did nothing for the rest of the
        // transfer, because the path had been seen before and was skipped.
        let local = Path::Default("en0".into());
        let phone = Path::Relay { relay: relay(), network: "cell".into() };
        let now = Arc::new(Mutex::new(vec![local.clone(), phone.clone()]));
        let lanes = watching(&[local.clone(), phone.clone()], Arc::clone(&now));

        // Both lanes healthy: nothing to do.
        assert!(lanes.joined(&[true, true]).is_empty());

        // Sharing goes off, the lane fails its chunks and is parked. The phone
        // is still listed, because the desktop remembers what it offered.
        let back = lanes.joined(&[true, false]);
        assert_eq!(back.len(), 1, "the phone was never brought back");
        assert_eq!(back[0].label, "Pixel (cell)");

        // And the replacement is not itself replaced on the next sweep, nor is
        // a third opened the moment the second one dies too.
        assert!(lanes.joined(&[true, false, true]).is_empty());
        assert!(lanes.joined(&[true, false, false]).is_empty(), "no wait before trying again");
    }

    #[test]
    fn a_path_nobody_offers_any_more_is_not_reopened() {
        // Unpairing a phone must not be undone by its lane then being parked.
        let local = Path::Default("en0".into());
        let phone = Path::Relay { relay: relay(), network: "cell".into() };
        let now = Arc::new(Mutex::new(vec![local.clone()]));
        let lanes = watching(&[local, phone], Arc::clone(&now));
        assert!(lanes.joined(&[true, false]).is_empty());
    }

    #[test]
    fn a_flapping_path_cannot_open_lanes_without_end() {
        let local = Path::Default("en0".into());
        let phone = Path::Relay { relay: relay(), network: "cell".into() };
        let now = Arc::new(Mutex::new(vec![local.clone(), phone.clone()]));
        let lanes = watching(&[local, phone], Arc::clone(&now));

        let mut live = vec![true, false];
        for _ in 0..MAX_LANES * 2 {
            let fresh = lanes.joined(&live);
            live.extend(std::iter::repeat_n(false, fresh.len()));
        }
        assert_eq!(live.len(), 3, "a dead path was retried on every sweep: {}", live.len());
    }

    #[test]
    fn a_set_that_is_not_watching_never_gains_a_lane() {
        // Every lane set but the app's own is fixed, and must stay that way.
        let lanes = build(&[Path::Default("en0".into())]).unwrap();
        assert!(lanes.joined(&[false]).is_empty());
    }

    #[test]
    fn one_lane_per_path_in_the_order_given() {
        let paths = vec![
            Path::Relay { relay: relay(), network: "cell".into() },
            Path::Relay { relay: relay(), network: "wifi".into() },
        ];
        let lanes = build(&paths).unwrap();
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes.label(0), "Pixel (cell)");
        assert_eq!(lanes.label(1), "Pixel (wifi)");
    }

    #[test]
    fn a_phone_contributes_as_many_lanes_as_it_offers() {
        // The USB case: one cable, two of the phone's networks, two lanes the
        // scheduler weighs independently.
        let paths = vec![
            Path::Default("en0".into()),
            Path::Relay { relay: relay(), network: "cell".into() },
            Path::Relay { relay: relay(), network: "wifi".into() },
        ];
        assert_eq!(build(&paths).unwrap().len(), 3);
    }

    #[test]
    fn the_default_lane_is_named_after_the_interface_it_will_use() {
        // Labelling it "default route" put a card in the sidebar that does not
        // exist, credited with every byte, while the real interface sat at
        // zero next to it and looked broken.
        let lanes = build(&[Path::Default("en0".into())]).unwrap();
        assert_eq!(lanes.label(0), "en0");
    }

    #[test]
    fn no_paths_is_an_error_rather_than_an_empty_lane_set() {
        // An empty LaneSet reads as "nothing to do" and would finish a
        // transfer that never started.
        assert!(build(&[]).is_err());
    }

    #[test]
    fn a_relay_that_is_down_still_produces_its_lane() {
        // Built, not probed. The selector parks a lane that fails.
        let paths = vec![Path::Relay { relay: relay(), network: "cell".into() }];
        assert!(build(&paths).is_ok());
    }

    #[test]
    fn an_interface_that_does_not_exist_is_an_error() {
        // A typo must not silently downgrade a transfer to fewer paths than
        // the person chose.
        assert!(build(&[Path::Interface("nope0".into())]).is_err());
    }

    #[tokio::test]
    async fn a_relay_lane_really_goes_through_the_relay() {
        // The assertion that matters. Without counting the hop this would only
        // prove that a client was built.
        let phone = dl_testkit::Relay::spawn().await.unwrap();
        let origin =
            dl_testkit::Origin::spawn(dl_testkit::Scenario::Ok200 { size: 4096 }).await.unwrap();

        let relay = Relay::new("Pixel", phone.addr().to_string(), None);
        let paths = vec![Path::Relay { relay, network: "cell".into() }];
        let lanes = PathLanes::build(
            &paths,
            &origin.url("payload.bin"),
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
        .unwrap();

        lanes.source(0).probe().await.expect("the relay forwards");
        assert!(phone.forwarded() > 0, "the request did not go through the relay");
    }

    #[tokio::test]
    async fn a_paired_key_satisfies_the_phone() {
        // End to end for pairing: the key pairing handed back, carried as
        // proxy credentials, is what stops the 407.
        let phone =
            dl_testkit::Relay::spawn_paired_phone("Pixel", Vec::new(), "secret").await.unwrap();
        let origin =
            dl_testkit::Origin::spawn(dl_testkit::Scenario::Ok200 { size: 4096 }).await.unwrap();

        let relay = Relay::new("Pixel", phone.addr().to_string(), Some("secret".into()));
        let paths = vec![Path::Relay { relay, network: "cell".into() }];
        let lanes = PathLanes::build(
            &paths,
            &origin.url("payload.bin"),
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
        .unwrap();

        lanes.source(0).probe().await.expect("a paired desktop is served");
    }

    #[tokio::test]
    async fn the_key_is_presented_on_a_tunnel_and_not_only_on_a_plain_request() {
        // Every download from an HTTPS origin begins with CONNECT, so if our
        // client sent credentials only on absolute-URI requests, every real
        // transfer through a phone would be refused while the tests passed.
        // Asked of the relay rather than of the response, because the upstream
        // here does not exist and the connection fails either way: the
        // question is whether we were challenged, not whether we connected.
        let phone =
            dl_testkit::Relay::spawn_paired_phone("Pixel", Vec::new(), "secret").await.unwrap();
        let relay = Relay::new("Pixel", phone.addr().to_string(), Some("secret".into()));
        let paths = vec![Path::Relay { relay, network: "cell".into() }];
        let lanes = PathLanes::build(
            &paths,
            "https://example.invalid/payload.bin",
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
        .unwrap();

        let _ = lanes.source(0).probe().await;
        assert_eq!(phone.challenged(), 0, "our client did not authenticate its CONNECT");
    }

    #[tokio::test]
    async fn a_phone_that_stops_recognising_us_costs_its_lane_and_not_the_file() {
        // Someone presses Forget on their phone mid-download. That lane starts
        // answering 407, and the transfer has to continue on the paths that
        // still work. Asserted at the classification because that is what the
        // chunk loop branches on: a retryable error requeues the chunk onto
        // another lane and fails this one towards being parked, while the
        // other arm ends the whole download.
        let refused = dl_core::error::Error::Http { status: 407 };
        assert!(refused.is_retryable(), "a proxy refusal must not fail the transfer");

        // The same for the status a phone sends when a lane it was offering
        // has gone away, which is the case its own UI produces when a network
        // is switched off.
        let gone = dl_core::error::Error::Http { status: 503 };
        assert!(gone.is_retryable(), "a lane going away must not fail the transfer");
    }

    #[tokio::test]
    async fn a_tunnel_without_a_key_is_challenged() {
        // The control case. Without it the assertion above could pass because
        // the relay never checks, which is exactly the gap it is testing for.
        let phone =
            dl_testkit::Relay::spawn_paired_phone("Pixel", Vec::new(), "secret").await.unwrap();
        let relay = Relay::new("Pixel", phone.addr().to_string(), None);
        let paths = vec![Path::Relay { relay, network: "cell".into() }];
        let lanes = PathLanes::build(
            &paths,
            "https://example.invalid/payload.bin",
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
        .unwrap();

        let _ = lanes.source(0).probe().await;
        assert!(phone.challenged() > 0, "an unpaired desktop opened a tunnel");
    }

    #[tokio::test]
    async fn an_unpaired_desktop_is_refused_by_a_phone_that_wants_a_key() {
        // The other half of the same claim. If this ever passes, the phone is
        // an open proxy and anyone nearby can spend its data.
        let phone =
            dl_testkit::Relay::spawn_paired_phone("Pixel", Vec::new(), "secret").await.unwrap();
        let origin =
            dl_testkit::Origin::spawn(dl_testkit::Scenario::Ok200 { size: 4096 }).await.unwrap();

        let relay = Relay::new("Pixel", phone.addr().to_string(), None);
        let paths = vec![Path::Relay { relay, network: "cell".into() }];
        let lanes = PathLanes::build(
            &paths,
            &origin.url("payload.bin"),
            &HttpConfig::default(),
            &FakeInterfaces(Vec::new()),
        )
        .unwrap();

        assert!(lanes.source(0).probe().await.is_err(), "an unpaired desktop was served");
    }
}
