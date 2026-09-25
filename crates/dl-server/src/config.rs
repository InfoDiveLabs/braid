//! Server configuration: a flat file the operator can hand-edit, and
//! environment variables a compose file can set instead. Never both required
//! at once, because a container that refuses to start without a file that
//! nobody wrote is a container nobody can start.

use dl_core::store::Durability;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Every field name this format understands, doubling as the file's key and,
/// upper-cased with a `BRAID_` prefix, the environment variable.
///
/// One list rather than two independent sets of match arms, so a field added
/// to the file and forgotten in the environment (or the other way round) is
/// not something that compiles quietly and only shows up as a report that a
/// setting from the compose file was ignored.
const FIELDS: &[&str] = &[
    "web_port",
    "download_dir",
    "config_dir",
    "torrent_port",
    "max_concurrent",
    "connections",
    "download_limit",
    "upload_limit",
    "auth_required",
    "interfaces",
    "interface_limits",
    "durability",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub web_port: u16,
    pub download_dir: PathBuf,
    pub config_dir: PathBuf,
    pub torrent_port: u16,
    pub max_concurrent: usize,
    pub connections: usize,
    pub download_limit: Option<u64>,
    pub upload_limit: Option<u64>,
    pub auth_required: bool,
    /// Interfaces a download should be spread across, named exactly as the
    /// OS names them. Empty means what it always has here: let the OS pick
    /// the route. See `paths_for` in `main.rs` for what this becomes.
    pub interfaces: Vec<String>,
    /// Ceilings for individual interfaces, by device name. Mirrors
    /// `dl_core::EngineConfig::interface_limits` exactly: this is where that
    /// value lives between restarts, since `EngineConfig` itself is rebuilt
    /// fresh every start rather than read off disk directly.
    pub interface_limits: BTreeMap<String, u64>,
    pub durability: Durability,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            web_port: 8080,
            download_dir: PathBuf::from("/downloads"),
            config_dir: PathBuf::from("/config"),
            torrent_port: 6881,
            // Matches `dl_core::EngineConfig`'s own default: a number picked
            // once for the desktop app because it kept a spinning disk busy
            // without thrashing it, and nothing about running headlessly
            // changes that trade-off.
            max_concurrent: 3,
            connections: 8,
            download_limit: None,
            upload_limit: None,
            // Naming none is not an oversight to fix later: most servers sit
            // on one well connected link where aggregation buys nothing, and
            // the OS already picks the right route for that case.
            interfaces: Vec::new(),
            // A server bound to a container's network namespace is reachable
            // from anything else in it, and from the host if the port is
            // published. Defaulting to open would mean the first honest
            // `docker run -p` exposes an unauthenticated file store to
            // whatever else is on that network.
            auth_required: true,
            interface_limits: BTreeMap::new(),
            durability: Durability::default(),
        }
    }
}

impl Config {
    /// Defaults, then the file, then the environment: each later source wins
    /// only where it actually says something.
    ///
    /// `env` is a closure rather than `std::env::var` read directly, because
    /// the process environment is one set of global mutable variables shared
    /// by every test in the binary. A test that called `std::env::set_var`
    /// would pass alone and fail, or worse pass flakily, whenever the test
    /// runner happened to run it beside another test doing the same thing:
    /// the classic suite that is green in isolation and red under `cargo
    /// test`. A closure makes the environment an ordinary input instead, and
    /// nothing here touches the real one.
    pub fn read(dir: &Path, env: impl Fn(&str) -> Option<String>) -> Config {
        let mut config = Config::default();

        // A missing or unreadable file is silently kept at defaults rather
        // than reported: the documented compose example never writes one, and
        // erroring here would fail every container's first boot.
        if let Ok(text) = std::fs::read_to_string(dir.join("server.conf")) {
            config.apply_file(&text);
        }

        for field in FIELDS {
            let variable = format!("BRAID_{}", field.to_uppercase());
            if let Some(value) = env(&variable) {
                config.apply(field, value.trim());
            }
        }

        config
    }

    fn apply_file(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            self.apply(key.trim(), value.trim());
        }
    }

    /// One field, from either source. A value that does not parse is left
    /// exactly as it was, rather than falling back to some other default:
    /// a typo in one line of a compose file should cost that one setting,
    /// not turn every other override in the same file into a coin flip about
    /// which format failure clears it.
    fn apply(&mut self, key: &str, value: &str) {
        match key {
            "web_port" => {
                if let Ok(v) = value.parse() {
                    self.web_port = v;
                }
            }
            "download_dir" => {
                if !value.is_empty() {
                    self.download_dir = PathBuf::from(value);
                }
            }
            "config_dir" => {
                if !value.is_empty() {
                    self.config_dir = PathBuf::from(value);
                }
            }
            "torrent_port" => {
                if let Ok(v) = value.parse() {
                    self.torrent_port = v;
                }
            }
            "max_concurrent" => {
                if let Ok(v) = value.parse() {
                    self.max_concurrent = v;
                }
            }
            "connections" => {
                if let Ok(v) = value.parse() {
                    self.connections = v;
                }
            }
            "download_limit" => self.download_limit = parse_limit(value, self.download_limit),
            "upload_limit" => self.upload_limit = parse_limit(value, self.upload_limit),
            "auth_required" => match value {
                "true" => self.auth_required = true,
                "false" => self.auth_required = false,
                _ => {}
            },
            "interfaces" => self.interfaces = parse_interfaces(value),
            "interface_limits" => self.interface_limits = parse_interface_limits(value),
            "durability" => {
                if let Some(d) = Durability::parse(value) {
                    self.durability = d;
                }
            }
            _ => {}
        }
    }

    /// Rewrite `server.conf` in `dir` with the settings held here.
    ///
    /// A full rewrite rather than a patch: the settings screen is the only
    /// writer this file ever has once a server is running, so there is no
    /// hand-written comment or forgotten field to merge back in. Written to a
    /// temporary name and renamed into place so a crash mid-write leaves the
    /// previous, still-valid file rather than a half-written one the next
    /// start would silently fall back to defaults from.
    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        let limits: Vec<String> =
            self.interface_limits.iter().map(|(name, rate)| format!("{name}:{rate}")).collect();
        let text = format!(
            "web_port = {}\n\
             download_dir = {}\n\
             config_dir = {}\n\
             torrent_port = {}\n\
             max_concurrent = {}\n\
             connections = {}\n\
             download_limit = {}\n\
             upload_limit = {}\n\
             auth_required = {}\n\
             interface_limits = {}\n\
             durability = {}\n",
            self.web_port,
            self.download_dir.display(),
            self.config_dir.display(),
            self.torrent_port,
            self.max_concurrent,
            self.connections,
            self.download_limit.map(|v| v.to_string()).unwrap_or_default(),
            self.upload_limit.map(|v| v.to_string()).unwrap_or_default(),
            self.auth_required,
            limits.join(","),
            self.durability.as_str(),
        );
        let temp = dir.join("server.conf.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, dir.join("server.conf"))
    }
}

/// `"en0:5000000,en1:0"`, comma separated `name:bytes-per-second` pairs. A
/// rate of zero is dropped rather than kept as an explicit zero-rate entry:
/// see `parse_limit` just below for why zero and absent mean the same thing.
fn parse_interface_limits(value: &str) -> BTreeMap<String, u64> {
    value
        .split(',')
        .filter_map(|pair| {
            let (name, rate) = pair.split_once(':')?;
            let rate: u64 = rate.trim().parse().ok()?;
            (rate != 0).then(|| (name.trim().to_string(), rate))
        })
        .collect()
}

/// Comma separated interface names, trimmed of surrounding whitespace so a
/// compose file that wraps the line does not smuggle spaces into a name the
/// OS will never match. An empty value clears the list rather than being
/// rejected as unparsable: that is how a compose file says "let the OS
/// route" without omitting the key, and it matches what an unset key already
/// defaults to.
fn parse_interfaces(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// An empty value means unlimited; zero means the same thing, since a rate
/// limit of zero bytes per second is not a rate anyone means to set. Anything
/// else that fails to parse keeps whatever was already there.
fn parse_limit(value: &str, previous: Option<u64>) -> Option<u64> {
    if value.is_empty() {
        return None;
    }
    match value.parse::<u64>() {
        Ok(0) => None,
        Ok(v) => Some(v),
        Err(_) => previous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_wins_over_the_file_and_the_file_over_the_defaults() {
        // A compose file sets environment variables and never writes a config
        // file, so the environment has to be sufficient on its own. Somebody
        // who has edited the file by hand expects that to survive a restart,
        // so it has to beat the defaults.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.conf"), "web_port = 9000\nconnections = 4\n")
            .unwrap();

        let from_file = Config::read(dir.path(), |_| None);
        assert_eq!(from_file.web_port, 9000);
        assert_eq!(from_file.connections, 4);

        let overridden =
            Config::read(dir.path(), |k| (k == "BRAID_WEB_PORT").then(|| "7000".into()));
        assert_eq!(overridden.web_port, 7000, "the environment must win");
        assert_eq!(overridden.connections, 4, "and must not clear what it did not set");
    }

    #[test]
    fn a_missing_config_file_is_not_an_error() {
        // First run in a fresh container. Refusing to start without a file
        // would make the documented compose example fail.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::read(dir.path(), |_| None).web_port, 8080);
    }

    #[test]
    fn a_nonsense_value_keeps_the_default_rather_than_stopping_the_server() {
        // A typo in a compose file must not leave somebody with a container
        // that will not boot and no obvious reason why.
        let dir = tempfile::tempdir().unwrap();
        let c = Config::read(dir.path(), |k| (k == "BRAID_WEB_PORT").then(|| "banana".into()));
        assert_eq!(c.web_port, 8080);
    }

    #[test]
    fn a_settings_change_survives_being_written_and_read_back() {
        // The settings screen writes through `write` and the next start reads
        // through `read`; if those two disagree about the format a restart
        // silently reverts whatever was just changed.
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config { durability: Durability::Safe, ..Default::default() };
        config.interface_limits.insert("en0".into(), 5_000_000);
        config.interface_limits.insert("en1".into(), 1_000_000);
        config.download_dir = dir.path().join("downloads");
        config.connections = 12;
        config.write(dir.path()).unwrap();

        let back = Config::read(dir.path(), |_| None);
        assert_eq!(back.durability, Durability::Safe);
        assert_eq!(back.interface_limits.get("en0"), Some(&5_000_000));
        assert_eq!(back.interface_limits.get("en1"), Some(&1_000_000));
        assert_eq!(back.download_dir, dir.path().join("downloads"));
        assert_eq!(back.connections, 12);
    }

    #[test]
    fn an_interface_limit_of_zero_is_the_same_as_no_limit_at_all() {
        // Matches `parse_limit`'s own rule for the global limit: a rate of
        // zero is not a rate anyone means to set, it is "take the checkbox
        // off", and keeping a zero entry around would have every reader of
        // `interface_limits` re-learn that a zero here means unlimited.
        assert_eq!(
            parse_interface_limits("en0:0,en1:5000"),
            BTreeMap::from([("en1".into(), 5000)])
        );
    }

    #[test]
    fn garbage_in_one_interface_limit_does_not_take_the_rest_down_with_it() {
        assert_eq!(
            parse_interface_limits("en0:5000,not-a-pair,en1:oops,en2:9000"),
            BTreeMap::from([("en0".into(), 5000), ("en2".into(), 9000)])
        );
    }

    #[test]
    fn an_unknown_durability_in_the_file_keeps_the_default_rather_than_refusing_to_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.conf"), "durability = overclocked\n").unwrap();
        assert_eq!(Config::read(dir.path(), |_| None).durability, Durability::default());
    }
}
