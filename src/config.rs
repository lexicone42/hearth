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
    /// EcoFlow source. Omit the whole `[ecoflow]` section to disable it; the
    /// poll loop then never touches EcoFlow.
    #[serde(default)]
    pub ecoflow: Option<EcoflowConfig>,
    /// Dyson sources (local MQTT push). Each `[[dyson]]` block is one device;
    /// omit them all to disable Dyson (no MQTT tasks spawned).
    #[serde(default)]
    pub dyson: Vec<DysonConfig>,
    /// Local HTTP API sink (`GET /api/latest` for LAN dashboards, e.g. the
    /// Wear OS tile). Omit the whole `[api]` section to disable it.
    #[serde(default)]
    pub api: Option<ApiConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    /// Bind address for the HTTP server. Defaults to `0.0.0.0:8091` (all
    /// interfaces — hearth assumes a trusted LAN; bind `127.0.0.1:8091` to
    /// keep it host-local).
    #[serde(default = "default_api_listen")]
    pub listen: String,
    /// Optional static bearer token. When set, `/api/latest` requires
    /// `Authorization: Bearer <token>`; `/healthz` stays open.
    #[serde(default)]
    pub token: Option<String>,
}

fn default_api_listen() -> String {
    "0.0.0.0:8091".to_string()
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
    /// Devices to READ BACK from SmartThings and surface as observations — e.g.
    /// a Wi-Fi lock cloud-linked to your account. Makes SmartThings a source,
    /// not just a sink. Empty = read nothing.
    #[serde(default)]
    pub read: Vec<ReadDevice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadDevice {
    /// SmartThings device id to poll (`GET /devices/{id}/status`).
    pub device_id: String,
    /// Canonical entity node this device's channels hang under, e.g.
    /// `front_door` -> `smartthings.front_door.lock` / `.battery`.
    pub node: String,
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
pub struct EcoflowConfig {
    /// EcoFlow IoT Open API "accessKey" (from the developer portal).
    pub access_key: String,
    /// EcoFlow IoT Open API "secretKey" (from the developer portal).
    pub secret_key: String,
    /// Device serial numbers to poll. Leave empty to discover them from the
    /// device-list endpoint at startup (still a no-op if both keys are unset,
    /// because the whole `[ecoflow]` section is then absent).
    #[serde(default)]
    pub device_sns: Vec<String>,
    /// API base host; defaults to `api-e.ecoflow.com`.
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DysonConfig {
    /// Device IP/hostname on the LAN, e.g. `192.168.2.133`.
    pub host: String,
    /// MQTT port. `1883` for plaintext (most models), `8883` for TLS models.
    #[serde(default = "default_dyson_port")]
    pub port: u16,
    /// Use TLS for the MQTT connection (the `8883` path on newer models).
    /// Defaults to false (plaintext `1883`).
    #[serde(default)]
    pub tls: bool,
    /// Setup SSID off the device sticker, `DYSON-<serial>-<product_type>`. The
    /// serial and numeric product type are parsed from it (libdyson's split).
    #[serde(default)]
    pub ssid: Option<String>,
    /// Wi-Fi password off the sticker. The MQTT credential is derived as
    /// `base64(SHA-512(wifi_password))` (libdyson `get_mqtt_info_from_wifi_info`).
    #[serde(default)]
    pub wifi_password: Option<String>,
    /// Explicit serial override (use when the sticker SSID is unavailable).
    #[serde(default)]
    pub serial: Option<String>,
    /// Explicit numeric product-type override, e.g. `438` (Pure Cool).
    #[serde(default)]
    pub product_type: Option<String>,
    /// Explicit MQTT credential override (the already-derived password). Lets
    /// you supply the credential directly instead of `wifi_password`.
    #[serde(default)]
    pub credential: Option<String>,
}

fn default_dyson_port() -> u16 {
    1883
}

#[derive(Debug, Clone, Deserialize)]
pub struct PollConfig {
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_interval(),
        }
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
