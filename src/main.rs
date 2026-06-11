mod ambient;
mod config;
mod domain;
mod smartthings;

use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use crate::ambient::client::AmbientClient;
use crate::config::Config;
use crate::smartthings::SmartThingsSink;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config_path =
        std::env::var("HEARTH_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {config_path}"))?;

    // Subcommand dispatch: `auth` runs the one-time OAuth flow, then exits.
    if let Some(cmd) = std::env::args().nth(1) {
        match cmd.as_str() {
            "auth" => return smartthings::auth::run_interactive(&config).await,
            "provision" => return smartthings::provision::run_provision(&config).await,
            other => {
                eprintln!("unknown command '{other}' (expected: auth, provision)");
                std::process::exit(2);
            }
        }
    }

    info!(
        interval_secs = config.poll.interval_secs,
        unit_system = ?config.unit_system,
        smartthings = config.smartthings.is_some(),
        mac = %config.ambient.mac_address,
        "starting ambient-st-bridge"
    );

    let client = AmbientClient::new(&config.ambient.application_key, &config.ambient.api_key)?;
    let sink = match &config.smartthings {
        Some(st) => Some(SmartThingsSink::new(st, config.unit_system)?),
        None => None,
    };

    run(client, sink, config).await
}

/// Phase 1+2+3a loop: poll Ambient Weather, normalize to canonical
/// `Observation`s, log them, and (when configured) push them to SmartThings.
/// Phase 3b puts an OAuth-refreshing token behind the sink; Phase 5 swaps the
/// REST poll for the realtime feed. Neither changes anything in this loop.
async fn run(client: AmbientClient, sink: Option<SmartThingsSink>, config: Config) -> Result<()> {
    let mut ticker = tokio::time::interval(Duration::from_secs(config.poll.interval_secs));
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match client.latest(&config.ambient.mac_address).await {
                    Ok(Some(reading)) => {
                        let observations = ambient::canonical::to_observations(&reading);
                        info!(count = observations.len(), "mapped observations");
                        for obs in &observations {
                            debug!(
                                entity = %obs.entity,
                                class = ?obs.class,
                                value = %obs.value.in_system(config.unit_system),
                                "observation",
                            );
                        }
                        if let Some(sink) = &sink {
                            sink.publish(&observations).await;
                        }
                    }
                    Ok(None) => warn!("station returned no observations"),
                    Err(e) => error!(error = ?e, "failed to fetch observation"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal received, exiting");
                break;
            }
        }
    }
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hearth=debug"));
    fmt().with_env_filter(filter).init();
}
