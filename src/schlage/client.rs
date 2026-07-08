use tracing::debug;

use crate::schlage::SchlageError;
use crate::schlage::auth::CognitoAuth;
use crate::schlage::model::{self, Device};

/// Schlage cloud API base (verified against pyschlage `auth.py::BASE_URL`).
const DEFAULT_BASE_URL: &str = "https://api.allegion.yonomi.cloud/v1";

/// Static API key every Schlage request must carry as `X-Api-Key` (verified
/// against pyschlage `auth.py::API_KEY`). This is an app key baked into the
/// Schlage app, not an account secret.
const API_KEY: &str = "hnuu9jbbJr7MssFDWm5nU2Z7nG5Q5rxsaqWsE7e9";

/// Thin client over Schlage's (unofficial) cloud API. Holds the Cognito SRP
/// authenticator, an HTTP client, and the API key. Every request carries the
/// `X-Api-Key` header and the Cognito access token as `Authorization: Bearer`.
/// All methods return typed [`SchlageError`]s so callers can log the right level.
pub struct SchlageClient {
    http: reqwest::Client,
    auth: CognitoAuth,
    base_url: String,
}

impl SchlageClient {
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, SchlageError> {
        Self::with_base_url(username, password, DEFAULT_BASE_URL)
    }

    pub fn with_base_url(
        username: impl Into<String>,
        password: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, SchlageError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(SchlageError::transport)?;
        Ok(Self {
            http,
            auth: CognitoAuth::new(username, password)?,
            base_url: base_url.into(),
        })
    }

    /// The authenticated user's id: `GET {base}/users/@me` -> `identityId`.
    ///
    /// Not needed by the poll loop (which only calls [`Self::locks`]), but part
    /// of the client's surface — mirrors pyschlage's `Auth.user_id` and is
    /// useful for future per-user filtering.
    #[allow(dead_code)]
    pub async fn user_id(&self) -> Result<String, SchlageError> {
        let body = self.get_text("users/@me", &[]).await?;
        model::parse_me(&body).map(|me| me.identity_id)
    }

    /// All locks on the account: `GET {base}/devices?archetype=lock` -> a JSON
    /// array of device objects (verified against pyschlage `api.py::locks`,
    /// `device.py::request_path`). A shape change in the response surfaces as
    /// [`SchlageError::Decode`] via [`model::parse_devices`].
    pub async fn locks(&self) -> Result<Vec<Device>, SchlageError> {
        let body = self.get_text("devices", &[("archetype", "lock")]).await?;
        model::parse_devices(&body)
    }

    /// GET `{base}/{path}` with the given query params, returning the raw body
    /// text (parsed by the caller so the decode path is testable in isolation).
    /// Attaches the `X-Api-Key` header and a fresh Cognito bearer token. A
    /// non-2xx status becomes [`SchlageError::Http`]; a network failure becomes
    /// [`SchlageError::Transport`].
    async fn get_text(&self, path: &str, params: &[(&str, &str)]) -> Result<String, SchlageError> {
        let token = self.auth.token().await?;
        debug!(path, "Schlage: fetching");
        let url = format!("{}/{path}", self.base_url);
        let resp = self
            .http
            .get(url)
            .header("X-Api-Key", API_KEY)
            .bearer_auth(token)
            .query(params)
            .send()
            .await
            .map_err(SchlageError::transport)?;
        let status = resp.status();
        // Read the body regardless of status: on success it's the payload, on
        // failure it's diagnostic detail carried by the Http error.
        let body = resp.text().await.map_err(SchlageError::transport)?;
        if !status.is_success() {
            return Err(SchlageError::from_http(status.as_u16(), body));
        }
        Ok(body)
    }
}
