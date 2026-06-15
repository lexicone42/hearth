use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::info;

use crate::ambient::canonical::to_observations;
use crate::ambient::client::AmbientClient;
use crate::config::Config;
use crate::domain::DeviceClass;
use crate::smartthings::capability::capability_id;
use crate::smartthings::client::SmartThingsClient;
use crate::smartthings::sink::token_source;

/// Persists provisioned device ids as a `device name -> device id` map.
pub struct DeviceStore {
    path: PathBuf,
}

impl DeviceStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<HashMap<String, String>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading device store {}", self.path.display()))?;
        serde_json::from_str(&text).context("parsing device store")
    }

    pub fn save(&self, map: &HashMap<String, String>) -> Result<()> {
        let text = serde_json::to_string_pretty(map).context("serializing device store")?;
        std::fs::write(&self.path, text)
            .with_context(|| format!("writing device store {}", self.path.display()))?;
        Ok(())
    }
}

/// Create the virtual devices declared in config that don't yet exist, deriving
/// each device's capabilities from the entities bound to it (discovered from a
/// live Ambient Weather reading). Idempotent: devices already in the store or
/// carrying an explicit `device_id` are skipped.
pub async fn run_provision(config: &Config) -> Result<()> {
    let st = config
        .smartthings
        .as_ref()
        .context("no [smartthings] section in config")?;

    // Discover which entities exist and their classes from a real reading.
    let aw = AmbientClient::new(&config.ambient.application_key, &config.ambient.api_key)?;
    let reading = aw
        .latest(&config.ambient.mac_address)
        .await?
        .context("Ambient Weather returned no reading")?;
    let observations = to_observations(&reading);
    let class_by_entity: HashMap<&str, DeviceClass> = observations
        .iter()
        .map(|o| (o.entity.as_str(), o.class))
        .collect();

    let client = SmartThingsClient::new(st.base_url.clone(), token_source(st)?)?;
    let location_id = client.default_location_id().await?;
    info!(location_id = %location_id, "provisioning into location");

    let store = DeviceStore::new(st.device_store.clone());
    let mut ids = store.load()?;
    let mut created = 0usize;

    for device in &st.devices {
        if device.device_id.is_some() {
            println!("• '{}' has an explicit device_id — skipping", device.name);
            continue;
        }
        if let Some(id) = ids.get(&device.name) {
            println!("• '{}' already provisioned ({id}) — skipping", device.name);
            continue;
        }

        // Capabilities = the distinct standard capabilities of the bound
        // entities that actually appear in the current reading.
        let mut capabilities: Vec<&str> = Vec::new();
        for entity in &device.entities {
            // Ambient entity classes come from the live reading; push sources
            // (Dyson) have deterministic channel→class mappings, so fall back to
            // those when the entity isn't part of an Ambient reading.
            let class = class_by_entity
                .get(entity.as_str())
                .copied()
                .or_else(|| class_for_dyson_entity(entity));
            match class {
                Some(class) => match capability_id(class) {
                    Some(cap) if !capabilities.contains(&cap) => capabilities.push(cap),
                    Some(_) => {}
                    None => println!("    {entity}: no standard capability yet — skipped"),
                },
                None => println!(
                    "    {entity}: unknown entity (not in a reading or a known dyson channel) — skipped"
                ),
            }
        }
        if capabilities.is_empty() {
            println!(
                "• '{}': no mappable capabilities — not created",
                device.name
            );
            continue;
        }

        let component = device.component.as_deref().unwrap_or("main");
        let id = client
            .create_virtual_device(&device.name, &location_id, component, &capabilities)
            .await?;
        println!(
            "✓ created '{}' ({id}) with [{}]",
            device.name,
            capabilities.join(", ")
        );
        ids.insert(device.name.clone(), id);
        created += 1;
    }

    store.save(&ids)?;
    println!("\nProvisioned {created} new device(s). Run `cargo run` to start pushing.");
    Ok(())
}

/// Class of a `dyson.<serial>.<channel>` entity, derived from its channel name
/// without a live device connection — so a push-source device can be provisioned
/// offline. `None` for non-dyson entities.
fn class_for_dyson_entity(entity: &str) -> Option<DeviceClass> {
    let channel = entity.strip_prefix("dyson.")?.rsplit('.').next()?;
    crate::dyson::canonical::class_for_channel(channel)
}
