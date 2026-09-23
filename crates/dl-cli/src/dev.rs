//! Development commands. Available only with `--features devtools`.

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum DevCommand {
    /// List the adversarial origin scenarios.
    Scenarios,
    /// Serve a scenario locally and print its URL.
    Serve {
        /// Scenario name, from `dl dev scenarios`.
        name: String,
    },
}

#[cfg(feature = "devtools")]
pub async fn run(command: DevCommand) -> Result<()> {
    use dl_testkit::{Origin, Scenario};

    let catalogue = Scenario::catalogue();
    match command {
        DevCommand::Scenarios => {
            for scenario in catalogue {
                let outcome = match (scenario.requires_refresh(), scenario.expected_len()) {
                    (true, _) => {
                        format!("needs link refresh, {} bytes", scenario.size().unwrap_or(0))
                    }
                    (false, Some(len)) => format!("succeeds, {len} bytes"),
                    (false, None) => "must fail".to_string(),
                };
                println!("{:<26} {outcome}", scenario.name());
            }
            Ok(())
        }
        DevCommand::Serve { name } => {
            let Some(scenario) = catalogue.into_iter().find(|s| s.name() == name) else {
                anyhow::bail!("unknown scenario {name:?}; see `dl dev scenarios`");
            };
            let origin = Origin::spawn(scenario).await?;
            println!("{}", origin.url("payload.bin"));
            if scenario.requires_refresh() {
                println!("refresh endpoint: {}", origin.issue_url("payload.bin"));
            }
            if let Some(digest) = scenario.expected_digest() {
                println!("expected blake3: {}", digest.to_hex());
            }
            println!("serving {scenario}; press ctrl-c to stop");
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
    }
}

#[cfg(not(feature = "devtools"))]
pub async fn run(_command: DevCommand) -> Result<()> {
    anyhow::bail!("this build has no dev commands; rebuild with --features devtools")
}
