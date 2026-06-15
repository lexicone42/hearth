use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

use crate::config::{Config, OAuthConfig};

const DEFAULT_AUTHORIZE_URL: &str = "https://api.smartthings.com/oauth/authorize";
const DEFAULT_TOKEN_URL: &str = "https://auth-global.api.smartthings.com/oauth/token";
/// Refresh this many seconds before the access token actually expires.
const REFRESH_MARGIN_SECS: i64 = 300;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Persisted OAuth tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Epoch seconds at which `access_token` expires.
    pub expires_at: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

impl TokenResponse {
    /// SmartThings does not always re-issue a refresh token on refresh, so fall
    /// back to the previous one when the response omits it.
    fn into_tokens(self, prev_refresh: Option<&str>) -> Result<Tokens> {
        let refresh_token = self
            .refresh_token
            .or_else(|| prev_refresh.map(str::to_owned))
            .context("token response had no refresh_token and none was cached")?;
        Ok(Tokens {
            access_token: self.access_token,
            refresh_token,
            expires_at: now_secs() + self.expires_in,
        })
    }
}

/// Stateless OAuth operations against the SmartThings endpoints.
pub struct OAuthClient {
    http: reqwest::Client,
    cfg: OAuthConfig,
}

impl OAuthClient {
    pub fn new(cfg: OAuthConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building OAuth HTTP client")?;
        Ok(Self { http, cfg })
    }

    fn authorize_base(&self) -> &str {
        self.cfg
            .authorize_url
            .as_deref()
            .unwrap_or(DEFAULT_AUTHORIZE_URL)
    }

    fn token_url(&self) -> &str {
        self.cfg.token_url.as_deref().unwrap_or(DEFAULT_TOKEN_URL)
    }

    /// The URL the user opens in a browser to grant access.
    pub fn authorize_url(&self) -> Result<String> {
        let scopes = self.cfg.scopes.join(" ");
        let url = reqwest::Url::parse_with_params(
            self.authorize_base(),
            &[
                ("client_id", self.cfg.client_id.as_str()),
                ("response_type", "code"),
                ("scope", scopes.as_str()),
                ("redirect_uri", self.cfg.redirect_uri.as_str()),
            ],
        )
        .context("building authorize URL")?;
        Ok(url.into())
    }

    async fn post_token(
        &self,
        params: &[(&str, &str)],
        prev_refresh: Option<&str>,
    ) -> Result<Tokens> {
        self.http
            .post(self.token_url())
            .basic_auth(&self.cfg.client_id, Some(&self.cfg.client_secret))
            .form(params)
            .send()
            .await
            .context("requesting SmartThings token")?
            .error_for_status()
            .context("SmartThings token endpoint returned an error")?
            .json::<TokenResponse>()
            .await
            .context("decoding token response")?
            .into_tokens(prev_refresh)
    }

    /// One-time exchange of an authorization code for tokens.
    pub async fn exchange_code(&self, code: &str) -> Result<Tokens> {
        self.post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", self.cfg.redirect_uri.as_str()),
            ],
            None,
        )
        .await
    }

    /// Exchange a refresh token for a fresh access token.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        self.post_token(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ],
            Some(refresh_token),
        )
        .await
    }
}

/// Reads/writes the persisted token file.
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<Option<Tokens>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading token store {}", self.path.display()))?;
        let tokens = serde_json::from_str(&text).context("parsing token store")?;
        Ok(Some(tokens))
    }

    pub fn save(&self, tokens: &Tokens) -> Result<()> {
        let text = serde_json::to_string_pretty(tokens).context("serializing tokens")?;
        std::fs::write(&self.path, text)
            .with_context(|| format!("writing token store {}", self.path.display()))?;
        Ok(())
    }
}

/// Holds the live tokens and refreshes them just-in-time.
pub struct TokenManager {
    oauth: OAuthClient,
    store: TokenStore,
    state: Mutex<Tokens>,
}

impl TokenManager {
    /// Load persisted tokens; errors if none exist yet (run `auth` first).
    pub fn load(oauth: OAuthClient, store: TokenStore) -> Result<Arc<Self>> {
        let tokens = store.load()?.context(
            "no SmartThings tokens stored yet — run `ambient-st-bridge auth` once to authorize",
        )?;
        Ok(Arc::new(Self {
            oauth,
            store,
            state: Mutex::new(tokens),
        }))
    }

    /// Return a currently-valid access token, refreshing and persisting it if it
    /// is at or near expiry.
    pub async fn valid_access_token(&self) -> Result<String> {
        let mut guard = self.state.lock().await;
        if now_secs() >= guard.expires_at - REFRESH_MARGIN_SECS {
            info!("refreshing SmartThings access token");
            let refreshed = self.oauth.refresh(&guard.refresh_token).await?;
            self.store.save(&refreshed)?;
            *guard = refreshed;
        }
        Ok(guard.access_token.clone())
    }
}

/// A source of bearer access tokens for the SmartThings client: either a static
/// token (quick test) or an OAuth manager that refreshes itself.
#[derive(Clone)]
pub enum TokenSource {
    Static(String),
    OAuth(Arc<TokenManager>),
}

impl TokenSource {
    pub async fn access_token(&self) -> Result<String> {
        match self {
            TokenSource::Static(token) => Ok(token.clone()),
            TokenSource::OAuth(manager) => manager.valid_access_token().await,
        }
    }
}

/// One-time interactive authorization: print the URL, read the pasted code,
/// exchange it for tokens, and persist them.
pub async fn run_interactive(config: &Config) -> Result<()> {
    let st = config
        .smartthings
        .as_ref()
        .context("no [smartthings] section in config")?;
    let oauth_cfg = st
        .oauth
        .clone()
        .context("no [smartthings.oauth] section in config")?;
    let oauth = OAuthClient::new(oauth_cfg)?;
    let store = TokenStore::new(st.token_store.clone());

    println!(
        "\nOpen this URL in a browser and approve access:\n\n  {}\n",
        oauth.authorize_url()?
    );
    println!("You'll be redirected to your redirect_uri with a `?code=...` parameter.");
    print!("Paste the code value here: ");
    use std::io::Write;
    std::io::stdout().flush().ok();

    let mut code = String::new();
    std::io::stdin()
        .read_line(&mut code)
        .context("reading code from stdin")?;
    let code = code.trim();
    if code.is_empty() {
        bail!("no code entered");
    }

    let tokens = oauth.exchange_code(code).await?;
    store.save(&tokens)?;
    println!(
        "\n✓ Authorized. Tokens saved to {}.",
        st.token_store.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OAuthConfig;

    fn cfg() -> OAuthConfig {
        OAuthConfig {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            redirect_uri: "https://localhost/cb".into(),
            scopes: vec!["r:devices:*".into()],
            authorize_url: None,
            token_url: None,
        }
    }

    #[test]
    fn authorize_url_includes_params() {
        let client = OAuthClient::new(cfg()).unwrap();
        let url = client.authorize_url().unwrap();
        assert!(url.starts_with(DEFAULT_AUTHORIZE_URL));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("scope="));
    }

    #[test]
    fn refresh_token_falls_back_when_omitted() {
        let resp = TokenResponse {
            access_token: "new-access".into(),
            refresh_token: None,
            expires_in: 86_400,
        };
        let tokens = resp.into_tokens(Some("old-refresh")).unwrap();
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token, "old-refresh");
        assert!(tokens.expires_at > now_secs());
    }
}
