mod ambient;
mod api;
mod config;
mod domain;
mod dyson;
mod ecoflow;
mod smartthings;

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::ambient::client::AmbientClient;
use crate::config::Config;
use crate::domain::Observation;
use crate::ecoflow::client::EcoflowClient;
use crate::smartthings::SmartThingsSink;

/// Per-sample batch of observations carried on the internal event bus. Each
/// source produces one `Vec<Observation>` per sample (preserving the existing
/// per-publish sink batch semantics); the router fans each batch into the
/// sink(s). Sized to comfortably absorb a slow sink without back-pressuring the
/// source tasks under normal cadence.
const BUS_CAPACITY: usize = 64;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = std::env::var("HEARTH_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let config =
        Config::load(&config_path).with_context(|| format!("loading config from {config_path}"))?;

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
        api = config.api.is_some(),
        dyson = config.dyson.len(),
        mac = %config.ambient.mac_address,
        "starting ambient-st-bridge"
    );

    let client = AmbientClient::new(&config.ambient.application_key, &config.ambient.api_key)?;
    let sink = match &config.smartthings {
        Some(st) => Some(SmartThingsSink::new(st, config.unit_system)?),
        None => None,
    };

    // EcoFlow source is entirely optional: with no `[ecoflow]` section it's
    // `None` and the source task is never spawned. Building the client is
    // offline (just an HTTP client + stored keys); device SNs are resolved at
    // poll time.
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

/// Internal event-bus orchestration. Each source is its own task holding a
/// clone of the bus `Sender`; a single router task owns the sink(s) and the
/// `Receiver`. Data flows source-task → bus → router → sink, so a new push
/// source (Dyson, Phase 2) needs no tick and no sink wiring — it just sends.
///
/// Observable behavior is identical to the previous inline loop: the same fetch
/// cadence (Ambient/EcoFlow tickers), the same log lines, and the same
/// SmartThings publishes (the router is the *only* place sinks are called).
async fn run(
    client: AmbientClient,
    sink: Option<SmartThingsSink>,
    ecoflow: Option<(EcoflowClient, Vec<String>)>,
    config: Config,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Vec<Observation>>(BUS_CAPACITY);

    // ----- Local API sink (only when `[api]` is configured) -----
    // The store is written by the router and read by the HTTP task; the server
    // failing (e.g. port taken) is logged, never fatal — API trouble must not
    // take down the bridge.
    let state = config.api.as_ref().map(|_| api::StateStore::new());
    let api_server = match (&config.api, &state) {
        (Some(api_cfg), Some(store)) => Some(tokio::spawn({
            let (api_cfg, store, system) = (api_cfg.clone(), store.clone(), config.unit_system);
            async move {
                if let Err(e) = api::server::serve(api_cfg, store, system).await {
                    error!(error = ?e, "api server exited");
                }
            }
        })),
        _ => None,
    };

    // Router: the single owner of the sink(s) and the bus receiver. Every
    // observation batch from every source flows through here, and this is the
    // ONLY place `sink.publish` is called.
    let router = tokio::spawn(router(rx, sink, state));

    // ----- Ambient source task (always present) -----
    let ambient = tokio::spawn(run_ambient(
        client,
        config.ambient.mac_address.clone(),
        config.unit_system,
        config.poll.interval_secs,
        tx.clone(),
    ));

    // ----- EcoFlow source task (only when `[ecoflow]` is configured) -----
    let ecoflow = ecoflow.map(|(client, sns)| {
        tokio::spawn(run_ecoflow(
            client,
            sns,
            config.unit_system,
            config.poll.interval_secs,
            tx.clone(),
        ))
    });

    // ----- Dyson source tasks (one per `[[dyson]]` device) -----
    // Push sources: no tick — each produces on its own incoming MQTT publishes,
    // all feeding the same bus (entity ids namespace by serial, so they never
    // collide). A device that fails to build is skipped, never fatal (mirrors
    // EcoFlow's optionality).
    let mut dyson_handles = Vec::new();
    for cfg in &config.dyson {
        match dyson::source::DysonSource::from_config(cfg) {
            Ok(source) => {
                dyson_handles.push(tokio::spawn(source.run(config.unit_system, tx.clone())));
            }
            Err(e) => error!(error = ?e, "failed to build a Dyson source — skipping it"),
        }
    }

    // Drop our own sender clone so the router's `rx.recv()` can observe channel
    // closure if every source task ends (it normally runs until ctrl-c).
    drop(tx);

    // Wait for ctrl-c, then exit. The source/router tasks run until the process
    // ends; aborting them here keeps shutdown prompt and clean.
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown signal received, exiting");

    ambient.abort();
    if let Some(h) = ecoflow {
        h.abort();
    }
    for h in dyson_handles {
        h.abort();
    }
    if let Some(h) = api_server {
        h.abort();
    }
    router.abort();

    Ok(())
}

/// Router task: owns the sink(s) and the bus receiver. Drains the bus and
/// publishes each batch. The only caller of `sink.publish` — and, likewise,
/// the only writer of the API state store.
async fn router(
    mut rx: mpsc::Receiver<Vec<Observation>>,
    sink: Option<SmartThingsSink>,
    state: Option<api::StateStore>,
) {
    while let Some(batch) = rx.recv().await {
        // Local store first: a slow SmartThings publish must not delay the
        // freshness of `/api/latest` (recording is a sync map insert).
        if let Some(state) = &state {
            state.record(&batch);
        }
        if let Some(sink) = &sink {
            sink.publish(&batch).await;
        }
    }
    debug!("event bus closed; router exiting");
}

/// Ambient source task: ticks on the poll interval, fetches the latest reading,
/// maps it to canonical observations, logs them, and sends the batch onto the
/// bus. Behaviorally identical to the old inline Ambient branch (same cadence,
/// same `"mapped observations"` log, same per-observation debug lines).
async fn run_ambient(
    client: AmbientClient,
    mac_address: String,
    unit_system: domain::UnitSystem,
    interval_secs: u64,
    tx: mpsc::Sender<Vec<Observation>>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    loop {
        ticker.tick().await;
        match client.latest(&mac_address).await {
            Ok(Some(reading)) => {
                let observations = ambient::canonical::to_observations(&reading);
                info!(count = observations.len(), "mapped observations");
                for obs in &observations {
                    debug!(
                        entity = %obs.entity,
                        class = ?obs.class,
                        value = %obs.value.in_system(unit_system),
                        "observation",
                    );
                }
                if tx.send(observations).await.is_err() {
                    // Router gone (shutdown). Nothing more to do.
                    break;
                }
            }
            Ok(None) => warn!("station returned no observations"),
            Err(e) => error!(error = ?e, "failed to fetch observation"),
        }
    }
}

/// EcoFlow source task: ticks on the poll interval and, for each configured (or
/// discovered) device, fetches its quota, maps it, logs it, and sends the batch
/// onto the bus. Errors are logged, never fatal — EcoFlow trouble must not take
/// down the bridge. Behaviorally identical to the old inline `poll_ecoflow`,
/// except batches now flow onto the bus instead of straight to the sink.
async fn run_ecoflow(
    client: EcoflowClient,
    configured_sns: Vec<String>,
    unit_system: domain::UnitSystem,
    interval_secs: u64,
    tx: mpsc::Sender<Vec<Observation>>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    loop {
        ticker.tick().await;

        // Resolve the device list: explicit config wins, else discover.
        let sns: Vec<String> = if configured_sns.is_empty() {
            match client.device_list().await {
                Ok(devices) => devices.into_iter().map(|d| d.sn).collect(),
                Err(e) => {
                    error!(error = ?e, "failed to fetch EcoFlow device list");
                    continue;
                }
            }
        } else {
            configured_sns.clone()
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
                    if tx.send(observations).await.is_err() {
                        // Router gone (shutdown).
                        return;
                    }
                }
                Err(e) => error!(sn = %sn, error = ?e, "failed to fetch EcoFlow quota"),
            }
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,hearth=debug"));
    fmt().with_env_filter(filter).init();
}
