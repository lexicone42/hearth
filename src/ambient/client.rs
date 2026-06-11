use anyhow::{Context, Result};

use crate::ambient::model::AwReading;

const DEFAULT_BASE_URL: &str = "https://api.ambientweather.net/v1";

/// Thin client over the Ambient Weather REST API.
pub struct AmbientClient {
    http: reqwest::Client,
    base_url: String,
    application_key: String,
    api_key: String,
}

impl AmbientClient {
    pub fn new(application_key: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            application_key: application_key.into(),
            api_key: api_key.into(),
        })
    }

    /// Fetch the most recent observation for a station.
    ///
    /// `GET /devices/{mac}` returns a time-ordered array (newest first); we ask
    /// for `limit=1`. Note: Ambient Weather rate-limits to ~1 request/second
    /// per application key, which is exactly the pressure the Phase 5 realtime
    /// Socket.IO feed removes.
    pub async fn latest(&self, mac: &str) -> Result<Option<AwReading>> {
        let url = format!("{}/devices/{mac}", self.base_url);
        let records: Vec<AwReading> = self
            .http
            .get(url)
            .query(&[
                ("applicationKey", self.application_key.as_str()),
                ("apiKey", self.api_key.as_str()),
                ("limit", "1"),
            ])
            .send()
            .await
            .context("sending request to Ambient Weather")?
            .error_for_status()
            .context("Ambient Weather returned an error status")?
            .json()
            .await
            .context("decoding Ambient Weather response")?;
        Ok(records.into_iter().next())
    }
}
