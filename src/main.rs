mod ambient;
mod config;
mod domain;
mod ecoflow;
mod smartthings;

use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use crate::ambient::client::AmbientClient;
use crate::config::Config;
use crate::ecoflow::client::EcoflowClient;
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
        ecoflow = config.ecoflow.is_some(),
        mac = %config.ambient.mac_address,
        "starting ambient-st-bridge"
    );

    let client = AmbientClient::new(&config.ambient.application_key, &config.ambient.api_key)?;
    let sink = match &config.smartthings {
        Some(st) => Some(SmartThingsSink::new(st, config.unit_system)?),
        None => None,
    };

    // EcoFlow source is entirely optional: with no `[ecoflow]` section it's
    // `None` and the loop never touches it. Building the client is offline
    // (just an HTTP client + stored keys); device SNs are resolved at poll time.
    let ecoflow = match &config.ecoflow {
        Some(cfg) => {
            let base = cfg.base_url.clone();
            let client = match base {
                Some(url) => EcoflowClient::with_base_url(&cfg.access_key, &cfg.secret_key, url)?,
                None => EcoflowClient::new(&cfg.access_key, &cfg.secret_key)?,
            };
            Some((client, cfg.device_sns.clone()))
        }
        None => None,
    };

    run(client, sink, ecoflow, config).await
}

/// Phase 1+2+3a loop: poll Ambient Weather, normalize to canonical
/// `Observation`s, log them, and (when configured) push them to SmartThings.
/// Phase 3b puts an OAuth-refreshing token behind the sink; Phase 5 swaps the
/// REST poll for the realtime feed. Neither changes anything in this loop.
async fn run(
    client: AmbientClient,
    sink: Option<SmartThingsSink>,
    ecoflow: Option<(EcoflowClient, Vec<String>)>,
    config: Config,
) -> Result<()> {
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

                // EcoFlow source: only runs when `[ecoflow]` is configured.
                if let Some((client, sns)) = &ecoflow {
                    poll_ecoflow(client, sns, &sink, config.unit_system).await;
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

/// Poll each configured EcoFlow device's quota, map it to canonical
/// observations, log them, and (when a sink is present) publish. Discovers
/// device SNs from the device-list endpoint when none are configured. Errors
/// are logged, never fatal — EcoFlow trouble must not take down the bridge.
async fn poll_ecoflow(
    client: &EcoflowClient,
    configured_sns: &[String],
    sink: &Option<SmartThingsSink>,
    unit_system: domain::UnitSystem,
) {
    // Resolve the device list: explicit config wins, else discover.
    let sns: Vec<String> = if configured_sns.is_empty() {
        match client.device_list().await {
            Ok(devices) => devices.into_iter().map(|d| d.sn).collect(),
            Err(e) => {
                error!(error = ?e, "failed to fetch EcoFlow device list");
                return;
            }
        }
    } else {
        configured_sns.to_vec()
    };

    for sn in &sns {
        match client.quota_all(sn).await {
            Ok(quota) => {
                let observations = ecoflow::canonical::to_observations(sn, &quota);
                info!(sn = %sn, count = observations.len(), "mapped EcoFlow observations");
                for obs in &observations {
                    debug!(
                        entity = %obs.entity,
                        class = ?obs.class,
                        value = %obs.value.in_system(unit_system),
                        "observation",
                    );
                }
                if let Some(sink) = sink {
                    sink.publish(&observations).await;
                }
            }
            Err(e) => error!(sn = %sn, error = ?e, "failed to fetch EcoFlow quota"),
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hearth=debug"));
    fmt().with_env_filter(filter).init();
}
