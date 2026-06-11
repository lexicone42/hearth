use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::domain::UnitSystem;

/// Top-level configuration, loaded from a TOML file (default `config.toml`,
/// override with the `AMBIENT_ST_CONFIG` env var). The `[smartthings]`
/// section lands here in Phase 4.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub ambient: AmbientConfig,
    #[serde(default)]
    pub poll: PollConfig,
    /// Unit system used when rendering/exporting values. Defaults to imperial.
    #[serde(default)]
    pub unit_system: UnitSystem,
    /// SmartThings sink. Omit the whole `[smartthings]` section to just log.
    #[serde(default)]
    pub smartthings: Option<SmartThingsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SmartThingsConfig {
    /// Static bearer token (a 24h PAT) for quick tests. Prefer `oauth` for a
    /// durable, self-refreshing setup. One of the two is required.
    #[serde(default)]
    pub access_token: Option<String>,
    /// OAuth client + flow; when present the sink refreshes tokens itself.
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
    /// Where refreshed OAuth tokens are persisted. Defaults to `token_store.json`.
    #[serde(default = "default_token_store")]
    pub token_store: PathBuf,
    /// Where provisioned virtual-device ids are persisted (name -> id).
    /// Defaults to `device_store.json`.
    #[serde(default = "default_device_store")]
    pub device_store: PathBuf,
    /// API base URL; defaults to `https://api.smartthings.com`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Virtual devices and the canonical entities each one carries.
    #[serde(default)]
    pub devices: Vec<DeviceBinding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// Override the authorize endpoint (defaults to SmartThings').
    #[serde(default)]
    pub authorize_url: Option<String>,
    /// Override the token endpoint (defaults to SmartThings').
    #[serde(default)]
    pub token_url: Option<String>,
}

fn default_token_store() -> PathBuf {
    PathBuf::from("token_store.json")
}

fn default_device_store() -> PathBuf {
    PathBuf::from("device_store.json")
}

fn default_scopes() -> Vec<String> {
    vec![
        "r:devices:*".to_string(),
        "w:devices:*".to_string(),
        "x:devices:*".to_string(),
    ]
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceBinding {
    /// Friendly device name; also the key a provisioned device id is stored under.
    pub name: String,
    /// Existing virtual device id. Leave unset to have `provision` create one.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Component to write to; defaults to `main`.
    #[serde(default)]
    pub component: Option<String>,
    /// Canonical entity ids routed to this device, e.g.
    /// `ambient_weather.outdoor.temperature`.
    #[serde(default)]
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AmbientConfig {
    /// Ambient Weather "applicationKey" (identifies this app).
    pub application_key: String,
    /// Ambient Weather "apiKey" (identifies your account).
    pub api_key: String,
    /// Station MAC address, e.g. "AA:BB:CC:DD:EE:FF".
    pub mac_address: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PollConfig {
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self { interval_secs: default_interval() }
    }
}

fn default_interval() -> u64 {
    60
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        toml::from_str(&text).context("parsing config TOML")
    }
}
