//! Server configuration: a flat file the operator can hand-edit, and
//! environment variables a compose file can set instead. Never both required
//! at once, because a container that refuses to start without a file that
//! nobody wrote is a container nobody can start.

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
            // A server bound to a container's network namespace is reachable
            // from anything else in it, and from the host if the port is
            // published. Defaulting to open would mean the first honest
            // `docker run -p` exposes an unauthenticated file store to
            // whatever else is on that network.
            auth_required: true,
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
            _ => {}
        }
    }
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
}
