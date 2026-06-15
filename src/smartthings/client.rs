use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::smartthings::auth::TokenSource;
use crate::smartthings::capability::StEvent;

const DEFAULT_BASE_URL: &str = "https://api.smartthings.com";

/// Minimal SmartThings API client: virtual-device events + provisioning.
pub struct SmartThingsClient {
    http: reqwest::Client,
    base_url: String,
    tokens: TokenSource,
}

#[derive(Serialize)]
struct DeviceEventsBody<'a> {
    #[serde(rename = "deviceEvents")]
    device_events: Vec<DeviceEventBody<'a>>,
}

#[derive(Serialize)]
struct DeviceEventBody<'a> {
    component: &'a str,
    capability: &'a str,
    attribute: &'a str,
    value: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<&'a str>,
}

impl SmartThingsClient {
    pub fn new(base_url: Option<String>, tokens: TokenSource) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building SmartThings HTTP client")?;
        Ok(Self {
            http,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            tokens,
        })
    }

    async fn token(&self) -> Result<String> {
        self.tokens.access_token().await
    }

    /// Push a batch of capability events to one virtual device:
    /// `POST {base}/virtualdevices/{deviceId}/events`.
    pub async fn send_events(
        &self,
        device_id: &str,
        component: &str,
        events: &[StEvent],
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let body = DeviceEventsBody {
            device_events: events
                .iter()
                .map(|e| DeviceEventBody {
                    component,
                    capability: e.capability,
                    attribute: e.attribute,
                    value: &e.value,
                    unit: e.unit,
                })
                .collect(),
        };
        let url = format!("{}/virtualdevices/{device_id}/events", self.base_url);
        self.http
            .post(url)
            .bearer_auth(self.token().await?)
            .json(&body)
            .send()
            .await
            .context("posting events to SmartThings")?
            .error_for_status()
            .context("SmartThings rejected the events POST")?;
        Ok(())
    }

    /// Derive a usable locationId from the account's existing devices — reuses
    /// the device-read scope instead of requiring a separate location scope.
    pub async fn default_location_id(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Devices {
            items: Vec<Dev>,
        }
        #[derive(Deserialize)]
        struct Dev {
            #[serde(rename = "locationId")]
            location_id: Option<String>,
        }
        let url = format!("{}/devices", self.base_url);
        let devices: Devices = self
            .http
            .get(url)
            .bearer_auth(self.token().await?)
            .send()
            .await
            .context("listing devices")?
            .error_for_status()
            .context("SmartThings rejected the devices list")?
            .json()
            .await
            .context("decoding devices")?;
        devices
            .items
            .into_iter()
            .find_map(|d| d.location_id)
            .context("could not determine a locationId from existing devices")
    }

    /// Create a virtual device with an inline device profile, returning its id:
    /// `POST {base}/virtualdevices`.
    pub async fn create_virtual_device(
        &self,
        name: &str,
        location_id: &str,
        component: &str,
        capabilities: &[&str],
    ) -> Result<String> {
        #[derive(Serialize)]
        struct Owner<'a> {
            #[serde(rename = "ownerType")]
            owner_type: &'a str,
            #[serde(rename = "ownerId")]
            owner_id: &'a str,
        }
        #[derive(Serialize)]
        struct Cap<'a> {
            id: &'a str,
            version: u8,
        }
        #[derive(Serialize)]
        struct Component<'a> {
            id: &'a str,
            capabilities: Vec<Cap<'a>>,
        }
        #[derive(Serialize)]
        struct Profile<'a> {
            name: String,
            components: Vec<Component<'a>>,
        }
        #[derive(Serialize)]
        struct CreateRequest<'a> {
            name: &'a str,
            owner: Owner<'a>,
            #[serde(rename = "deviceProfile")]
            device_profile: Profile<'a>,
        }
        #[derive(Deserialize)]
        struct Created {
            #[serde(rename = "deviceId")]
            device_id: String,
        }

        let body = CreateRequest {
            name,
            owner: Owner {
                owner_type: "LOCATION",
                owner_id: location_id,
            },
            device_profile: Profile {
                name: format!("{name} profile"),
                components: vec![Component {
                    id: component,
                    capabilities: capabilities
                        .iter()
                        .map(|&id| Cap { id, version: 1 })
                        .collect(),
                }],
            },
        };
        let url = format!("{}/virtualdevices", self.base_url);
        let created: Created = self
            .http
            .post(url)
            .bearer_auth(self.token().await?)
            .json(&body)
            .send()
            .await
            .context("creating virtual device")?
            .error_for_status()
            .context("SmartThings rejected virtual device creation")?
            .json()
            .await
            .context("decoding created device")?;
        Ok(created.device_id)
    }
}
