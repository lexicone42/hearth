use std::collections::HashMap;

use anyhow::Result;
use serde_json::json;
use tracing::{debug, info, warn};

use crate::config::SmartThingsConfig;
use crate::domain::{Observation, UnitSystem};
use crate::smartthings::auth::{OAuthClient, TokenManager, TokenSource, TokenStore};
use crate::smartthings::capability::{StEvent, to_event};
use crate::smartthings::client::SmartThingsClient;
use crate::smartthings::provision::DeviceStore;

/// Routes canonical observations to SmartThings virtual devices per the
/// configured bindings, mapping each to a standard capability event.
pub struct SmartThingsSink {
    client: SmartThingsClient,
    system: UnitSystem,
    /// entity id -> (device_id, component)
    routes: HashMap<String, (String, String)>,
}

impl SmartThingsSink {
    /// `client` is built once by `main` and shared with the read-back source,
    /// so both share one OAuth token manager.
    pub fn new(
        client: SmartThingsClient,
        config: &SmartThingsConfig,
        system: UnitSystem,
    ) -> Result<Self> {
        let provisioned = DeviceStore::new(config.device_store.clone())
            .load()
            .unwrap_or_default();

        let mut routes = HashMap::new();
        for device in &config.devices {
            let Some(device_id) = device
                .device_id
                .clone()
                .or_else(|| provisioned.get(&device.name).cloned())
            else {
                warn!(device = %device.name, "no device_id yet (run `provision`) — skipping its bindings");
                continue;
            };
            let component = device
                .component
                .clone()
                .unwrap_or_else(|| "main".to_string());
            for entity in &device.entities {
                routes.insert(entity.clone(), (device_id.clone(), component.clone()));
            }
        }
        Ok(Self {
            client,
            system,
            routes,
        })
    }

    /// Map every bound, standard-capability observation and push it, batched per
    /// device. Observations with no binding or no standard capability are
    /// counted (not silently dropped) and reported.
    pub async fn publish(&self, observations: &[Observation]) {
        let mut batches: HashMap<(String, String), Vec<StEvent>> = HashMap::new();
        let mut unbound = 0usize;
        let mut no_capability = 0usize;

        for obs in observations {
            let Some(route) = self.routes.get(obs.entity.as_str()) else {
                unbound += 1;
                continue;
            };
            match to_event(obs, self.system) {
                Some(event) => {
                    let batch = batches.entry(route.clone()).or_default();
                    // A freshly-created SmartThings virtual device reports garbage
                    // for any attribute we never set. For temperature that
                    // includes `temperatureRange`, which the Family Hub renders
                    // instead of the reading (huge bogus values). Pin a sane range
                    // in the reading's unit so the tile shows the real temperature.
                    if event.capability == "temperatureMeasurement"
                        && event.attribute == "temperature"
                    {
                        batch.push(StEvent {
                            capability: "temperatureMeasurement",
                            attribute: "temperatureRange",
                            value: json!({ "minimum": -40.0, "maximum": 120.0, "step": 0.1 }),
                            unit: event.unit,
                        });
                    }
                    batch.push(event);
                }
                None => no_capability += 1,
            }
        }

        let mut sent = 0usize;
        for ((device_id, component), events) in &batches {
            match self.client.send_events(device_id, component, events).await {
                Ok(()) => {
                    sent += events.len();
                    debug!(device_id, count = events.len(), "pushed events");
                }
                Err(e) => warn!(device_id, error = ?e, "failed to push events to SmartThings"),
            }
        }
        info!(sent, unbound, no_capability, "smartthings publish");
    }
}

/// Build the token source: OAuth (self-refreshing) when configured, else a
/// static token, else an error.
pub(crate) fn token_source(config: &SmartThingsConfig) -> Result<TokenSource> {
    if let Some(oauth_cfg) = &config.oauth {
        let oauth = OAuthClient::new(oauth_cfg.clone())?;
        let store = TokenStore::new(config.token_store.clone());
        Ok(TokenSource::OAuth(TokenManager::load(oauth, store)?))
    } else if let Some(token) = &config.access_token {
        Ok(TokenSource::Static(token.clone()))
    } else {
        anyhow::bail!(
            "smartthings config needs either `access_token` or an `[smartthings.oauth]` section"
        )
    }
}
