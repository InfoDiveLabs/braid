// Braid: a download manager that splits one file across every network path
// you have.
// Copyright (C) 2026 InfoDive Labs Pvt Ltd. <https://www.infodivelabs.com>
//
// This program is free software: you can redistribute it and/or modify it
// under the terms of the GNU General Public License version 3, as published
// by the Free Software Foundation.
//
// This program is distributed in the hope that it will be useful, but WITHOUT
// ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
// FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for
// more details.
//
// You should have received a copy of the GNU General Public License along
// with this program. If not, see <https://www.gnu.org/licenses/>.

//! `dl`: command-line interface to the download engine.

mod dev;
mod human;
mod schedule_runner;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use dl_core::refresh::{CommandRefresher, LinkRefresher, RefreshPolicy, StaticRefresher};
use dl_core::{
    ByteSource, DownloadOptions, FileStorage, LaneSet, ResumeOptions, download,
    download_over_lanes, model::ByteRange,
};
use dl_net::{
    HttpConfig, HttpSource, Interface, InterfaceLanes, InterfaceProvider, LaneSpec,
    RefreshingLanes, SystemInterfaces, filename_from_url, lane_specs,
};
use std::path::PathBuf;
use std::sync::Arc;

/// What `--version` says in full.
///
/// The licence asks a program with a terminal interface to say this where
/// someone will see it. There is no `show w` command to point at because the
/// warranty text is in LICENSE, which every package installs.
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\n",
    "Copyright (C) 2026 InfoDive Labs Pvt Ltd. <https://www.infodivelabs.com>\n",
    "Written by Suraj Tiwari.\n",
    "\n",
    "Braid is free software under the GNU General Public License version 3,\n",
    "and comes with ABSOLUTELY NO WARRANTY. See the LICENSE file distributed\n",
    "with it, or <https://www.gnu.org/licenses/gpl-3.0.html>."
);

#[derive(Parser)]
#[command(
    name = "dl",
    version,
    long_version = LONG_VERSION,
    about = "Parallel multi-interface download manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download a URL.
    Add(Box<AddArgs>),
    /// Fetch a URL's metadata without downloading it.
    Probe { url: String },
    /// Enumerate network interfaces and report the binding mechanism in effect.
    Interfaces,
    #[command(subcommand)]
    Dev(dev::DevCommand),
}

#[derive(clap::Args)]
struct AddArgs {
    url: String,

    /// Where to write. Defaults to the origin's suggested name, or the URL's.
    ///
    /// For a torrent this is the **folder** its files land in, not a filename:
    /// a torrent names its own contents and `dl` does not rename them.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Verify the result against a digest, as `sha256:<hex>`, `blake3:<hex>`,
    /// `md5:<hex>`, or a bare digest.
    #[arg(long)]
    verify: Option<String>,

    /// How hard to work to survive a power cut: `safe` flushes on every
    /// completed chunk, `balanced` on a 5 second or 64 MB floor, `fast` leaves
    /// the journal flush to the operating system.
    ///
    /// Looser modes never risk a wrong file: only more re-downloading after a
    /// crash.
    #[arg(long, value_name = "MODE", default_value = "balanced")]
    durability: String,

    /// Fetch only the first N bytes. Accepts suffixes: 512K, 16M, 2G.
    ///
    /// A deliberate prefix for testing large files, not a truncation: the
    /// length check still applies. Requires the origin to support ranges.
    #[arg(long, value_name = "SIZE")]
    head: Option<String>,

    /// Overwrite the destination if it already exists.
    #[arg(long)]
    force: bool,

    /// Parallel connections per download.
    #[arg(short = 'n', long, default_value_t = 8)]
    connections: usize,

    /// Chunk size. Chosen from the file size and connection count if omitted.
    #[arg(long, value_name = "SIZE")]
    chunk_size: Option<String>,

    /// On resume, re-read existing chunks and re-fetch any that fail their hash.
    #[arg(long)]
    verify_existing: bool,

    /// Spread the download across these interfaces, e.g. `en0,en5`.
    #[arg(long, value_name = "NAMES", value_delimiter = ',')]
    interfaces: Vec<String>,

    /// Spread the download across every interface that has a gateway.
    #[arg(long, conflicts_with = "interfaces")]
    all_interfaces: bool,

    /// Cap this download's throughput. Accepts suffixes: 500K, 2M.
    #[arg(long, value_name = "RATE")]
    limit: Option<String>,

    /// Cap one interface, as `name=rate`. Repeatable, e.g. for a metered link.
    #[arg(long = "limit-interface", value_name = "NAME=RATE")]
    limit_interfaces: Vec<String>,

    /// Apply a limit only during a window, as `HH:MM-HH:MM=RATE`, or
    /// `HH:MM-HH:MM=off` for unlimited. Repeatable.
    #[arg(long = "at", value_name = "WINDOW")]
    windows: Vec<String>,

    /// Download in one stream with no journal, so nothing can be resumed.
    #[arg(long)]
    no_resume: bool,

    /// Another URL for the same file. Repeatable; each becomes a lane.
    ///
    /// Mirrors must agree on length and validator. One serving different
    /// content is dropped rather than spliced into the file.
    #[arg(long = "mirror", value_name = "URL")]
    mirrors: Vec<String>,

    /// Command that prints a fresh URL when the link expires, as a bare URL or
    /// as `{"url": ..., "headers": {...}}`. For example `yt-dlp -g`.
    ///
    /// This runs a program of your choosing, so it is off unless asked for.
    /// The command is executed directly, never through a shell.
    #[arg(long = "refresh-command", value_name = "CMD")]
    refresh_command: Option<String>,
}

/// Download a torrent, from a magnet link, a `.torrent` URL or a local file.
///
/// `--output` names the **folder** the torrent's own files land in, not a
/// filename: a torrent names its contents and this is not the place to rename
/// them. Seeding stops when the last piece arrives, which is the behaviour a
/// command that has to return expects: the GUI is where a transfer can sit in
/// a swarm indefinitely.
#[cfg(feature = "torrent")]
async fn add_torrent(source: dl_core::TorrentSource, args: &AddArgs) -> Result<()> {
    use dl_core::TorrentBackend as _;

    let destination = args.output.clone().unwrap_or_else(|| PathBuf::from("."));
    tokio::fs::create_dir_all(&destination)
        .await
        .with_context(|| format!("creating {}", destination.display()))?;

    let limit = args.limit.as_deref().map(human::parse_size).transpose()?;
    let cancel = dl_core::Cancel::new();
    // Ctrl-C pauses rather than kills: every piece already written passed its
    // own hash, and the backend stops cleanly so the next run resumes.
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nstopping; run the same command again to resume");
                cancel.cancel();
            }
        }
    });

    println!("opening {}", args.url);
    println!("  writing into {}", destination.display());

    let backend = dl_torrent::LibrqbitBackend::new(dl_torrent::SessionConfig::new(&destination));
    // The trait takes an `Fn`, because several transfers share one backend and
    // the engine calls it from its own task. The bar needs `&mut self`, so it
    // sits behind a mutex rather than the signature being widened for one
    // caller that happens to be single-threaded.
    let bar = std::sync::Mutex::new(human::ProgressBar::new());
    let outcome = backend
        .run(dl_core::TorrentRequest {
            // `dl` has no remove command; a cancelled transfer keeps its
            // pieces so the next run resumes.
            delete_files: Default::default(),
            // `dl` runs one transfer at a time, so nothing else is competing
            // for the limit.
            other_traffic: Default::default(),
            source,
            destination: destination.clone(),
            cancel,
            download_limit: dl_core::budget::Budget::with_rate(limit.unwrap_or(0)),
            upload_limit: dl_core::budget::Budget::unlimited(),
            keep_partial: true,
            seed_after_complete: false,
            on_progress: Some(Box::new(move |report| {
                if let Ok(mut bar) = bar.lock() {
                    bar.update(report.progress);
                }
            })),
        })
        .await?;
    backend.shutdown().await;

    println!("\ndone  {}  {}", outcome.name, human::bytes(outcome.total));
    for file in &outcome.files {
        println!("  {:<48} {:>10}", file.path, human::bytes(file.len));
    }
    if outcome.uploaded > 0 {
        println!("uploaded {} while downloading", human::bytes(outcome.uploaded));
    }
    Ok(())
}

#[cfg(not(feature = "torrent"))]
async fn add_torrent(_source: dl_core::TorrentSource, args: &AddArgs) -> Result<()> {
    // Named precisely, because the alternative is someone concluding their
    // magnet link is broken.
    bail!(
        "{} is a torrent, and this build of dl has no torrent support; \
         rebuild with `cargo build -p dl-cli --features torrent`",
        args.url
    )
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        match Cli::parse().command {
            Command::Add(args) => add(*args).await,
            Command::Probe { url } => probe(&url).await,
            Command::Interfaces => interfaces(),
            Command::Dev(cmd) => dev::run(cmd).await,
        }
    })
}

async fn add(args: AddArgs) -> Result<()> {
    // Decided from the link alone, before anything is built: a magnet has no
    // origin to probe, and sending it down the HTTP path would report an
    // unparseable URL for a link that is perfectly well formed.
    match dl_core::classify(&args.url) {
        dl_core::TransferKind::Torrent(source) => return add_torrent(source, &args).await,
        dl_core::TransferKind::IncompleteMagnet => {
            bail!("{} carries no xt=urn:btih: info hash, so there is no torrent to find", args.url)
        }
        dl_core::TransferKind::Http => {}
    }

    let head = args.head.as_deref().map(human::parse_size).transpose()?;
    let expect = args.verify.as_deref().map(human::parse_digest).transpose()?;

    let source = HttpSource::with_config(&HttpConfig::default(), &args.url)?;

    // A refresher or a mirror turns every (url, interface) pair into its own
    // lane, because a signature can be bound to the address that asked for it
    // and so has to be resolved once per path.
    let interfaces = chosen_interfaces(&args)?;
    let urls: Vec<String> =
        std::iter::once(args.url.clone()).chain(args.mirrors.iter().cloned()).collect();
    let specs = if args.refresh_command.is_some() || urls.len() > 1 {
        lane_specs(&urls, &interfaces)
    } else {
        Vec::new()
    };
    let refreshing = match specs.is_empty() {
        true => None,
        false => Some(build_refreshing(&args, specs.clone())?),
    };

    eprintln!("probing {}", args.url);
    // Through a path that can refresh, where one exists: a link that has
    // already expired fails at the very first request, and probing around the
    // refresher would report it dead before the refresher ever ran.
    let info = match &refreshing {
        Some(lanes) => lanes.source(0).probe().await.context("probing the url")?,
        None => source.probe().await.context("probing the url")?,
    };

    let destination = match args.output.clone() {
        Some(path) => path,
        None => PathBuf::from(
            info.suggested_filename
                .clone()
                .or_else(|| filename_from_url(&info.final_url))
                .or_else(|| filename_from_url(&args.url))
                .unwrap_or_else(|| "download.bin".to_string()),
        ),
    };

    if destination.exists() && !args.force {
        bail!("{} already exists; pass --force to overwrite", destination.display());
    }
    if args.force {
        let _ = tokio::fs::remove_file(&destination).await;
    }

    let range = match head {
        // The prefix cannot be longer than the resource itself.
        Some(n) => Some(ByteRange::new(0, info.len.map_or(n, |len| n.min(len)))),
        None => None,
    };

    println!("{}", human::describe_source(&info, &args.url));
    if let Some(range) = range {
        println!("  fetching only the first {} (--head)", human::bytes(range.len()));
    }
    println!("  writing to {}", destination.display());

    // Resume needs ranges and a known length, and a prefix fetch is a one-shot
    // request by definition.
    let resumable = info.supports_chunking() && range.is_none() && !args.no_resume;

    // Built before the branch so a limit means the same thing on both paths.
    let limit = args.limit.as_deref().map(human::parse_size).transpose()?;
    let budget = dl_core::budget::Budget::with_rate(limit.unwrap_or(0));
    let schedule = human::parse_windows(&args.windows, limit)?;
    let scheduler = schedule
        .as_ref()
        .map(|s| schedule_runner::spawn(s.clone(), std::sync::Arc::clone(&budget)));
    if schedule.is_some() {
        println!("  schedule: {} window(s) active", args.windows.len());
    }

    if resumable {
        let bound = match (refreshing.is_some(), interfaces.is_empty()) {
            (false, false) => {
                Some(InterfaceLanes::new(&interfaces, &info.final_url, &HttpConfig::default())?)
            }
            _ => None,
        };
        if let Some(lanes) = &bound {
            describe_lanes(lanes);
        }
        if !specs.is_empty() {
            describe_specs(&args, &specs);
        }

        let single = dl_core::SingleLane::new(&source);
        let lane_set: &dyn dl_core::LaneSet = match (&refreshing, &bound) {
            (Some(lanes), _) => lanes,
            (None, Some(lanes)) => lanes,
            (None, None) => &single,
        };

        let lane_interfaces: Vec<Option<String>> = match (&refreshing, &bound) {
            (Some(_), _) => {
                specs.iter().map(|s| s.interface.as_ref().map(|i| i.name.clone())).collect()
            }
            (None, Some(lanes)) => {
                lanes.lanes().iter().map(|l| Some(l.interface.name.clone())).collect()
            }
            (None, None) => Vec::new(),
        };
        let lane_limits = lane_caps(&args, &lane_interfaces)?;

        let outcome = download_over_lanes(
            lane_set,
            &destination,
            ResumeOptions {
                chunk_size: args.chunk_size.as_deref().map(human::parse_size).transpose()?,
                durability: dl_core::store::Durability::parse(&args.durability).ok_or_else(
                    || anyhow::anyhow!("unknown durability mode {:?}", args.durability),
                )?,
                connections: args.connections,
                expect: expect.clone(),
                verify_existing: args.verify_existing,
                limit: Some(std::sync::Arc::clone(&budget)),
                lane_limits,
                ..Default::default()
            },
            Some(Box::new({
                let mut bar = human::ProgressBar::new();
                move |p| bar.update(p)
            })),
        )
        .await?;

        println!(
            "\ndone  {}  in {}  ({}/s)",
            human::bytes(outcome.total),
            human::duration(outcome.elapsed),
            human::bytes(rate(outcome.transferred, outcome.elapsed)),
        );
        println!(
            "{} connections, {} chunks of {}",
            outcome.connections,
            outcome.chunks,
            human::bytes(outcome.chunk_size)
        );
        if outcome.lanes.len() > 1 {
            println!("per-interface:");
            for lane in &outcome.lanes {
                let share = if outcome.total > 0 {
                    lane.bytes as f64 / outcome.total as f64 * 100.0
                } else {
                    0.0
                };
                println!(
                    "  {:<28} {:>9}  {:>10}/s  {:>5.1}%{}",
                    lane.label,
                    human::bytes(lane.bytes),
                    human::bytes(lane.throughput.unwrap_or(0.0) as u64),
                    share,
                    if lane.parked { "  (dropped out)" } else { "" }
                );
            }
        }
        if !outcome.repaired.is_empty() {
            println!(
                "re-fetched {} chunk(s) that failed verification: {:?}",
                outcome.repaired.len(),
                outcome.repaired
            );
        }
        if outcome.resumed_from > 0 {
            println!(
                "resumed from {}; transferred {}",
                human::bytes(outcome.resumed_from),
                human::bytes(outcome.transferred)
            );
        }
        if let Some(digest) = &outcome.verified {
            println!("{:<7} {}", digest.algorithm().as_str(), digest.to_hex());
            println!("verified against the supplied digest");
        }
        if let Some(scheduler) = scheduler {
            scheduler.abort();
        }
        return Ok(());
    }

    let storage = FileStorage::create(&destination).await?;
    let mut bar = human::ProgressBar::new();
    // One-shot downloads refresh too; the link is no less likely to expire for
    // the transfer not being resumable.
    let single: &dyn ByteSource = match &refreshing {
        Some(lanes) => lanes.source(0),
        None => &source,
    };
    let outcome = download(
        single,
        &storage,
        DownloadOptions {
            expect: expect.clone(),
            range,
            budget: dl_core::budget::BudgetChain::new(vec![std::sync::Arc::clone(&budget)]),
            ..Default::default()
        },
        Some(Box::new(move |p| bar.update(p))),
    )
    .await?;

    println!(
        "\ndone  {}  in {}  ({}/s)",
        human::bytes(outcome.bytes),
        human::duration(outcome.elapsed),
        human::bytes(outcome.bytes_per_sec()),
    );
    println!("blake3  {}", outcome.blake3.to_hex());
    if expect.is_some() {
        println!("verified against the supplied digest");
    }
    if let Some(scheduler) = scheduler {
        scheduler.abort();
    }
    Ok(())
}

fn rate(bytes: u64, elapsed: std::time::Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 { 0 } else { (bytes as f64 / secs) as u64 }
}

async fn probe(url: &str) -> Result<()> {
    let source = HttpSource::with_config(&HttpConfig::default(), url)?;
    let info = source.probe().await.context("probing the url")?;
    println!("{}", human::describe_source(&info, url));
    Ok(())
}

fn interfaces() -> Result<()> {
    use dl_net::{Family, InterfaceProvider, SystemInterfaces, bind_to_interface};
    use socket2::{Domain, Protocol, Socket, Type};

    let all = SystemInterfaces.interfaces();
    println!(
        "{:<10} {:>5} {:<18} {:<8} {:<20} state",
        "iface", "idx", "address", "gateway", "binding"
    );

    for iface in all {
        let address = iface
            .routable_ipv4()
            .map(|a| a.to_string())
            .or_else(|| iface.ipv4.first().map(|a| a.to_string()))
            .or_else(|| iface.ipv6.first().map(|a| a.to_string()))
            .unwrap_or_else(|| "-".into());

        // Report the mechanism actually in effect rather than the one we hope
        // for. A silent fallback to address-only binding looks identical to
        // success until bandwidth fails to aggregate across interfaces.
        let binding = match Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)) {
            Ok(socket) => match bind_to_interface(&socket, &iface, Family::V4) {
                Ok(m) if m.is_authoritative() => m.as_str().to_string(),
                Ok(m) => format!("{} (weak)", m.as_str()),
                Err(e) => format!("unavailable: {e}"),
            },
            Err(e) => format!("socket failed: {e}"),
        };

        // Reports what the downloader would actually do with this interface,
        // not just what the kernel says about it.
        let state = if iface.is_loopback {
            "loopback"
        } else if !iface.is_up {
            "down"
        } else if !iface.has_routable_address() {
            "link-local only"
        } else if !iface.has_gateway {
            "no gateway"
        } else {
            "usable"
        };

        println!(
            "{:<10} {:>5} {:<20} {:<8} {:<16} {}",
            iface.name,
            iface.index,
            truncate(&address, 20),
            if iface.has_gateway { "yes" } else { "no" },
            binding,
            state
        );
    }
    Ok(())
}

/// The interfaces this download should use, or none if it was not asked to
/// spread across any.
///
/// A misspelled name is an error: quietly dropping it would leave the user
/// believing they were aggregating across interfaces when they were not.
fn chosen_interfaces(args: &AddArgs) -> Result<Vec<Interface>> {
    if args.all_interfaces {
        return Ok(SystemInterfaces
            .usable()
            .into_iter()
            .filter(|i| i.has_gateway && i.has_routable_address())
            .collect());
    }
    args.interfaces
        .iter()
        .map(|name| {
            SystemInterfaces
                .by_name(name)
                .ok_or_else(|| anyhow::anyhow!("no interface named {name:?}"))
        })
        .collect()
}

fn build_refreshing(args: &AddArgs, specs: Vec<LaneSpec>) -> Result<RefreshingLanes> {
    let refresher: Arc<dyn LinkRefresher> = match &args.refresh_command {
        Some(command) => Arc::new(CommandRefresher::parse(command)?),
        None => Arc::new(StaticRefresher),
    };
    Ok(RefreshingLanes::build(
        specs,
        &HttpConfig::default(),
        RefreshPolicy::default(),
        refresher,
        &|_, _| None,
    )?)
}

fn describe_specs(args: &AddArgs, specs: &[LaneSpec]) {
    if let Some(command) = &args.refresh_command {
        // Worth saying out loud: this is a program the downloader will run.
        println!("  link refresh: running {command:?} when the link expires");
    }
    println!("  paths:");
    for spec in specs {
        println!("    {:<22} {}", spec.label, spec.initial.url);
    }
}

/// Report what each lane is actually pinned to before the transfer starts.
///
/// A lane that fell back to address-only binding still works, but its traffic
/// is not constrained to its interface, so any apparent aggregation would just
/// be extra connections over one route.
fn describe_lanes(lanes: &InterfaceLanes) {
    println!("  interfaces:");
    for lane in lanes.lanes() {
        let address =
            lane.interface.ipv4.first().map(|a| a.to_string()).unwrap_or_else(|| "-".into());
        println!(
            "    {:<10} {:<16} binding: {}",
            lane.interface.name,
            address,
            lane.binding.as_str()
        );
    }
    if !lanes.all_scoped() {
        eprintln!(
            "warning: at least one interface is not scoped, so its traffic may leave by the \n\
             default route. Combined throughput may not exceed a single interface."
        );
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.len() <= width { value.to_string() } else { format!("{}…", &value[..width - 1]) }
}

/// Build a per-lane budget list from `--limit-interface name=rate`.
///
/// An unknown name is an error: silently ignoring it would leave the user
/// believing a metered link was capped when it was not.
///
/// One interface can carry several lanes once mirrors are in play, and they
/// share a single budget rather than getting one each: a ceiling on a metered
/// link means the link, not each connection over it.
fn lane_caps(
    args: &AddArgs,
    lane_interfaces: &[Option<String>],
) -> Result<Vec<Option<Arc<dl_core::budget::Budget>>>> {
    if args.limit_interfaces.is_empty() {
        return Ok(Vec::new());
    }
    if lane_interfaces.iter().all(Option::is_none) {
        bail!("--limit-interface needs --interfaces or --all-interfaces");
    }

    let mut caps: Vec<Option<Arc<dl_core::budget::Budget>>> = vec![None; lane_interfaces.len()];

    for spec in &args.limit_interfaces {
        let (name, rate) = spec
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("expected NAME=RATE, got {spec:?}"))?;
        let budget = dl_core::budget::Budget::with_rate(human::parse_size(rate)?);

        let mut matched = false;
        for (index, lane) in lane_interfaces.iter().enumerate() {
            if lane.as_deref() == Some(name) {
                caps[index] = Some(Arc::clone(&budget));
                matched = true;
            }
        }
        if !matched {
            bail!("{name:?} is not one of the interfaces in use");
        }
    }
    Ok(caps)
}
