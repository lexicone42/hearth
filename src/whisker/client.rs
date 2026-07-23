use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;
use tracing::debug;

use crate::whisker::WhiskerError;
use crate::whisker::auth::{self, CognitoAuth};
use crate::whisker::model::{self, Pet, Robot};

// TODO(phase 2): activity feed for weight-trend history + native low-litter /
// drawer-full alert events — `GET https://ub.prod.iothings.site/robots/{serial}/activities`
// returns PET_VISIT (petId/petWeight/wasteType) plus DRAWER_FULL / LITTER_LOW /
// LITTER_CRITICALLY_LOW events. Not built yet; wire it as a third source later.

/// Litter-Robot 5 robots endpoint (live-verified). Returns a JSON array of the
/// account's robots; requires BOTH the id-token bearer AND the `x-api-key`.
const ROBOTS_ENDPOINT: &str = "https://ub.prod.iothings.site/robots";

/// Whisker pet-profile GraphQL endpoint (live-verified). Per-cat weights; needs
/// only the id-token bearer.
const PETS_ENDPOINT: &str = "https://pet-profile.iothings.site/graphql/";

/// The public Whisker API-gateway key, base64-encoded exactly as pylitterbot
/// ships it (`API_V2_ENDPOINT_KEY`). Decoded at runtime to a 40-char ASCII key
/// sent as `x-api-key` on the robots endpoint. This is a public app constant
/// (same category as the Cognito pool/client ids), NOT a per-user secret.
const API_V2_ENDPOINT_KEY_B64: &str = "cDduZE1vajYxbnBSWlA1Q1Z6OXY0VWowYkc3Njl4eTY3NThRUkJQYg==";

/// The pet-profile query: the per-cat weights (the hub owner's #1 goal).
const QUERY_PETS_BY_USER: &str = "\
query GetPetsByUser($userId: String!) {
  getPetsByUser(userId: $userId) {
    petId
    name
    type
    weight
    lastWeightReading
    weightLastUpdated
    weightIdFeatureEnabled
  }
}";

/// Thin client over Whisker's (unofficial) Litter-Robot 5 cloud. Holds the
/// Cognito SRP authenticator (id-token bearer), an HTTP client, and the decoded
/// `x-api-key`. All methods return typed [`WhiskerError`]s.
pub struct WhiskerClient {
    http: reqwest::Client,
    auth: CognitoAuth,
    /// Decoded (40-char) Whisker API-gateway key for the robots endpoint.
    api_key: String,
    /// Optional `[whisker].serial`: when set, only that box is surfaced.
    serial: Option<String>,
    /// Robots REST base (overridable in tests).
    robots_url: String,
    /// Pet-profile GraphQL base (overridable in tests).
    pets_url: String,
}

impl WhiskerClient {
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
        serial: Option<String>,
    ) -> Result<Self, WhiskerError> {
        Self::with_endpoints(username, password, serial, ROBOTS_ENDPOINT, PETS_ENDPOINT)
    }

    pub fn with_endpoints(
        username: impl Into<String>,
        password: impl Into<String>,
        serial: Option<String>,
        robots_url: impl Into<String>,
        pets_url: impl Into<String>,
    ) -> Result<Self, WhiskerError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(WhiskerError::transport)?;
        // The base64 is a hardcoded public constant, so decoding cannot fail at
        // runtime; treat a failure as a decode error rather than panicking.
        let api_key_bytes = BASE64
            .decode(API_V2_ENDPOINT_KEY_B64)
            .map_err(|e| WhiskerError::decode(format!("API_V2_ENDPOINT_KEY not base64: {e}")))?;
        let api_key = String::from_utf8(api_key_bytes)
            .map_err(|e| WhiskerError::decode(format!("API_V2_ENDPOINT_KEY not UTF-8: {e}")))?;
        Ok(Self {
            http,
            auth: CognitoAuth::new(username, password)?,
            api_key,
            // Treat an empty configured serial as "not set" (surface all boxes).
            serial: serial.filter(|s| !s.trim().is_empty()),
            robots_url: robots_url.into(),
            pets_url: pets_url.into(),
        })
    }

    /// The authenticated user's id — the `mid` claim decoded from the id token.
    /// Needed as the `userId` variable for the pet-profile query.
    pub async fn user_id(&self) -> Result<String, WhiskerError> {
        let token = self.auth.token().await?;
        auth::mid_from_id_token(&token)
    }

    /// The account's Litter-Robots (`GET /robots`, Bearer + `x-api-key`). When
    /// `[whisker].serial` is configured, the result is filtered to that one box.
    pub async fn list_robots(&self) -> Result<Vec<Robot>, WhiskerError> {
        let token = self.auth.token().await?;
        debug!("Whisker: fetching robots");
        let resp = self
            .http
            .get(&self.robots_url)
            .bearer_auth(&token)
            .header("x-api-key", &self.api_key)
            .send()
            .await
            .map_err(WhiskerError::transport)?;
        let text = read_body(resp).await?;
        let robots = model::parse_robots(&text)?;
        Ok(match &self.serial {
            Some(serial) => robots.into_iter().filter(|r| &r.serial == serial).collect(),
            None => robots,
        })
    }

    /// The account's pets and their weights (`getPetsByUser`, Bearer). `user_id`
    /// is the `mid` claim (see [`Self::user_id`]).
    pub async fn list_pets(&self, user_id: &str) -> Result<Vec<Pet>, WhiskerError> {
        let token = self.auth.token().await?;
        debug!("Whisker: fetching pets");
        let body = json!({
            "query": QUERY_PETS_BY_USER,
            "variables": { "userId": user_id },
        });
        let resp = self
            .http
            .post(&self.pets_url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(WhiskerError::transport)?;
        let text = read_body(resp).await?;
        model::parse_pets(&text)
    }
}

/// Read a response body, mapping the status to a typed error. A `401` with a
/// token attached (we always attach one) means the token was rejected —
/// [`WhiskerError::Credentials`] (verified live: a missing/expired token 401s).
/// Any other non-2xx keeps the raw status+body as [`WhiskerError::Http`].
async fn read_body(resp: reqwest::Response) -> Result<String, WhiskerError> {
    let status = resp.status();
    // Read the body regardless of status: on success it's the payload, on
    // failure it's diagnostic detail carried by the error.
    let text = resp.text().await.map_err(WhiskerError::transport)?;
    if status.is_success() {
        return Ok(text);
    }
    if status.as_u16() == 401 {
        return Err(WhiskerError::Credentials);
    }
    Err(WhiskerError::from_http(status.as_u16(), text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whisker::canonical::{pet_observations, robot_observations};

    /// End-to-end smoke test against the LIVE Whisker cloud: real Cognito SRP
    /// handshake → id token → `GET /robots` + `getPetsByUser` → canonical mapping.
    /// Ignored by default (needs real credentials + network). CI never runs it.
    /// After changing auth/endpoints, re-verify with:
    ///
    /// ```sh
    /// WHISKER_USER='you@example.com' WHISKER_PASS='...' \
    ///   cargo test whisker::client::tests::live_smoke -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "hits the live Whisker cloud; set WHISKER_USER / WHISKER_PASS and pass --ignored"]
    async fn live_smoke() {
        let user = std::env::var("WHISKER_USER").expect("WHISKER_USER");
        let pass = std::env::var("WHISKER_PASS").expect("WHISKER_PASS");
        let client = WhiskerClient::new(user, pass, None).expect("build client");

        // Drives the SRP handshake and the `mid` claim decode.
        let uid = client
            .user_id()
            .await
            .expect("user_id (SRP auth + mid decode)");
        eprintln!("\n[live] authenticated; user_id (mid) = {uid}");

        let robots = client.list_robots().await.expect("list_robots");
        eprintln!("[live] {} robot(s):", robots.len());
        for r in &robots {
            for o in robot_observations(r) {
                eprintln!("   {} = {:?}", o.entity.as_str(), o.value);
            }
        }

        let pets = client.list_pets(&uid).await.expect("list_pets");
        eprintln!("[live] {} pet(s):", pets.len());
        for p in &pets {
            for o in pet_observations(p) {
                eprintln!("   {} = {:?}", o.entity.as_str(), o.value);
            }
        }

        assert!(!robots.is_empty(), "expected at least one robot");
        assert!(!pets.is_empty(), "expected at least one pet");
    }
}
