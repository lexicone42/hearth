//! AWS Cognito SRP authentication for the Schlage cloud.
//!
//! Schlage's app authenticates against an AWS Cognito user pool using the
//! Secure Remote Password (SRP) protocol, then carries the resulting Cognito
//! **access token** as a `Bearer` credential on every Schlage API request.
//!
//! We use [`aws_cognito_srp`] to do the SRP math (it generates the `SRP_A` /
//! password-claim parameters and, given a client secret, would compute a secret
//! hash) but make the two Cognito IDP calls ourselves as raw `x-amz-json-1.1`
//! POSTs with `reqwest` — this keeps deps lean (no `aws-sdk-cognitoidentityprovider`).
//!
//! ## Flow (verified against `dknowles2/pyschlage` + `pvizeli/pycognito`)
//!
//! 1. **InitiateAuth** (`USER_SRP_AUTH`): send `USERNAME`, `SRP_A`, and — because
//!    this app client has a secret — `SECRET_HASH`. Cognito replies with the
//!    `PASSWORD_VERIFIER` challenge (`SRP_B`, `SALT`, `SECRET_BLOCK`,
//!    `USER_ID_FOR_SRP`).
//! 2. **RespondToAuthChallenge** (`PASSWORD_VERIFIER`): send the computed
//!    `PASSWORD_CLAIM_SIGNATURE`, `PASSWORD_CLAIM_SECRET_BLOCK`, `TIMESTAMP`, the
//!    challenge's `USER_ID_FOR_SRP` as `USERNAME`, and a `SECRET_HASH` re-keyed
//!    on that `USER_ID_FOR_SRP`. Cognito replies with `AccessToken` / `IdToken`
//!    / `RefreshToken` / `ExpiresIn`.
//!
//! The **access token** is what goes in the Schlage `Authorization` header as
//! `Bearer <access_token>` — pyschlage builds its transport with pycognito's
//! `RequestsSrpAuth`, whose defaults are `http_header_prefix = "Bearer "` and
//! `auth_token_type = ACCESS_TOKEN`, and pyschlage does not override them.
//!
//! Failures are classified into [`SchlageError`] so the caller can log an
//! actionable message: a rejected login is [`SchlageError::Credentials`], an
//! MFA/new-password prompt is [`SchlageError::UnexpectedChallenge`], and a
//! Cognito response that doesn't match our expectations is
//! [`SchlageError::Decode`] (the "their API changed" signal).
//!
//! Tokens are held in memory only (no persistence — re-auth is cheap and
//! non-interactive, unlike SmartThings OAuth). On expiry we try a cheap
//! `REFRESH_TOKEN_AUTH`; if that fails, we fall back to a full SRP handshake.

use aws_cognito_srp::{SrpClient, User, UserAuthenticationParameters, VerificationParameters};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::schlage::SchlageError;

type HmacSha256 = Hmac<Sha256>;

// ---- Constants (verified against pyschlage `auth.py`) ----
const USER_POOL_ID: &str = "us-west-2_2zhrVs9d4";
const CLIENT_ID: &str = "t5836cptp2s1il0u9lki03j5";
const CLIENT_SECRET: &str = "1kfmt18bgaig51in4j4v1j3jbe7ioqtjhle5o6knqc5dat0tpuvo";

/// The regional Cognito IDP endpoint (`us-west-2`). All IDP actions POST here.
const COGNITO_IDP_URL: &str = "https://cognito-idp.us-west-2.amazonaws.com/";
const TARGET_INITIATE_AUTH: &str = "AWSCognitoIdentityProviderService.InitiateAuth";
const TARGET_RESPOND_TO_CHALLENGE: &str =
    "AWSCognitoIdentityProviderService.RespondToAuthChallenge";

/// The one SRP challenge we can answer headlessly.
const PASSWORD_VERIFIER: &str = "PASSWORD_VERIFIER";

/// Cognito `__type` exception suffixes that mean the login itself was rejected
/// (the user must fix their credentials). Mirrors pyschlage's
/// `_NOT_AUTHORIZED_ERRORS`.
const NOT_AUTHORIZED_TYPES: &[&str] = &[
    "NotAuthorizedException",
    "InvalidPasswordException",
    "PasswordResetRequiredException",
    "UserNotFoundException",
    "UserNotConfirmedException",
];

/// Refresh this many seconds *before* the access token actually expires, so an
/// in-flight request never carries a just-expired token.
const EXPIRY_MARGIN_SECS: i64 = 60;

/// Cognito SRP authenticator: owns the credentials and the current token state,
/// and hands out a valid access token on demand (refreshing / re-authing as
/// needed). `token()` takes `&self`; the token state lives behind a `Mutex` so
/// a single refresh is serialized rather than raced.
pub struct CognitoAuth {
    http: reqwest::Client,
    username: String,
    password: String,
    state: Mutex<Option<TokenState>>,
}

/// The in-memory token cache: the access token used for the Schlage API, the
/// refresh token to renew it cheaply, and when to consider the access token stale.
#[derive(Debug)]
struct TokenState {
    access_token: String,
    refresh_token: String,
    expires_at: Instant,
}

impl TokenState {
    /// Build from a Cognito `AuthenticationResult` object. Refresh responses
    /// omit `RefreshToken`, so `fallback_refresh` (the prior one) is reused then.
    /// A missing `AccessToken` is a [`SchlageError::Decode`] (shape drift).
    fn from_auth_result(
        auth: &serde_json::Value,
        fallback_refresh: Option<&str>,
    ) -> Result<Self, SchlageError> {
        let access_token = field_str(auth, "AccessToken")?.to_string();
        let refresh_token = match auth.get("RefreshToken").and_then(|v| v.as_str()) {
            Some(rt) => rt.to_string(),
            None => fallback_refresh
                .ok_or_else(|| {
                    SchlageError::decode("auth result has no RefreshToken and no prior token")
                })?
                .to_string(),
        };
        // Cognito access/id tokens default to 3600s; trust ExpiresIn when present.
        let expires_in = auth
            .get("ExpiresIn")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(3600);
        let ttl = expires_in.saturating_sub(EXPIRY_MARGIN_SECS).max(0) as u64;
        Ok(Self {
            access_token,
            refresh_token,
            expires_at: Instant::now() + Duration::from_secs(ttl),
        })
    }
}

impl CognitoAuth {
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, SchlageError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(SchlageError::transport)?;
        Ok(Self {
            http,
            username: username.into(),
            password: password.into(),
            state: Mutex::new(None),
        })
    }

    /// A currently-valid Cognito **access token**. Returns the cached token when
    /// fresh; otherwise refreshes (cheap) and, if that fails, does a full SRP
    /// re-auth. Errors are typed ([`SchlageError`]) so the caller can react.
    pub async fn token(&self) -> Result<String, SchlageError> {
        let mut guard = self.state.lock().await;

        if let Some(state) = guard.as_ref() {
            if Instant::now() < state.expires_at {
                return Ok(state.access_token.clone());
            }
            // Stale: try a cheap refresh first.
            let refresh_token = state.refresh_token.clone();
            debug!("Schlage: refreshing Cognito token");
            match self.refresh(&refresh_token).await {
                Ok(new_state) => {
                    let token = new_state.access_token.clone();
                    *guard = Some(new_state);
                    debug!("Schlage: token refreshed");
                    return Ok(token);
                }
                Err(e) => {
                    warn!(kind = e.kind(), error = %e, "Schlage: token refresh failed; re-authenticating")
                }
            }
        }

        debug!("Schlage: authenticating (Cognito SRP)");
        let new_state = self.full_authenticate().await?;
        let token = new_state.access_token.clone();
        *guard = Some(new_state);
        debug!("Schlage: authenticated");
        Ok(token)
    }

    /// The full `USER_SRP_AUTH` -> `PASSWORD_VERIFIER` handshake.
    async fn full_authenticate(&self) -> Result<TokenState, SchlageError> {
        let srp = SrpClient::new(
            User::new(USER_POOL_ID, &self.username, &self.password),
            CLIENT_ID,
            Some(CLIENT_SECRET),
        );
        let UserAuthenticationParameters { a, username } = srp.get_auth_parameters();

        // Part 1: InitiateAuth (USER_SRP_AUTH). SECRET_HASH is required because
        // the app client has a secret; for InitiateAuth it's keyed on the login
        // username.
        let init = self
            .cognito_idp(
                TARGET_INITIATE_AUTH,
                &json!({
                    "AuthFlow": "USER_SRP_AUTH",
                    "ClientId": CLIENT_ID,
                    "AuthParameters": {
                        "USERNAME": username,
                        "SRP_A": a,
                        "SECRET_HASH": secret_hash(&self.username),
                    },
                }),
            )
            .await?;

        // Cognito should answer with the PASSWORD_VERIFIER challenge; anything
        // else (MFA/new-password) we can't answer headlessly.
        require_password_verifier(&init)?;
        let params = init.get("ChallengeParameters").ok_or_else(|| {
            SchlageError::decode("InitiateAuth response had no ChallengeParameters")
        })?;
        let secret_block = field_str(params, "SECRET_BLOCK")?;
        let srp_b = field_str(params, "SRP_B")?;
        let salt = field_str(params, "SALT")?;
        let user_id_for_srp = field_str(params, "USER_ID_FOR_SRP")?;

        // Compute the password claim from the challenge (SRP verify). A failure
        // here means the challenge values themselves were malformed -> Decode.
        let VerificationParameters {
            password_claim_secret_block,
            password_claim_signature,
            timestamp,
        } = srp
            .verify(secret_block, user_id_for_srp, salt, srp_b)
            .map_err(|e| {
                SchlageError::decode(format!(
                    "SRP verify failed (bad Cognito challenge params?): {e}"
                ))
            })?;

        // Part 2: RespondToAuthChallenge (PASSWORD_VERIFIER). USERNAME is now
        // the challenge's USER_ID_FOR_SRP, and SECRET_HASH is re-keyed on it
        // (matches pycognito's process_challenge).
        let resp = self
            .cognito_idp(
                TARGET_RESPOND_TO_CHALLENGE,
                &json!({
                    "ChallengeName": PASSWORD_VERIFIER,
                    "ClientId": CLIENT_ID,
                    "ChallengeResponses": {
                        "USERNAME": user_id_for_srp,
                        "PASSWORD_CLAIM_SECRET_BLOCK": password_claim_secret_block,
                        "PASSWORD_CLAIM_SIGNATURE": password_claim_signature,
                        "TIMESTAMP": timestamp,
                        "SECRET_HASH": secret_hash(user_id_for_srp),
                    },
                }),
            )
            .await?;

        // A successful verify yields AuthenticationResult; a *further* challenge
        // (SMS_MFA, NEW_PASSWORD_REQUIRED, ...) is UnexpectedChallenge.
        let auth = expect_auth_result(&resp)?;
        TokenState::from_auth_result(auth, None)
    }

    /// Cheap token renewal via `REFRESH_TOKEN_AUTH`. The SECRET_HASH is keyed on
    /// the login username here (pycognito's `_add_secret_hash`). No full SRP.
    async fn refresh(&self, refresh_token: &str) -> Result<TokenState, SchlageError> {
        let resp = self
            .cognito_idp(
                TARGET_INITIATE_AUTH,
                &json!({
                    "AuthFlow": "REFRESH_TOKEN_AUTH",
                    "ClientId": CLIENT_ID,
                    "AuthParameters": {
                        "REFRESH_TOKEN": refresh_token,
                        "SECRET_HASH": secret_hash(&self.username),
                    },
                }),
            )
            .await?;

        let auth = expect_auth_result(&resp)?;
        // Refresh responses don't carry a new RefreshToken; reuse the current one.
        TokenState::from_auth_result(auth, Some(refresh_token))
    }

    /// One raw Cognito IDP action: a `x-amz-json-1.1` POST with the given
    /// `X-Amz-Target`. Non-2xx bodies are classified (auth exceptions ->
    /// `Credentials`, else `Http`); a 2xx body that isn't JSON is `Decode`;
    /// a network failure is `Transport`.
    async fn cognito_idp(
        &self,
        target: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, SchlageError> {
        let resp = self
            .http
            .post(COGNITO_IDP_URL)
            .header(reqwest::header::CONTENT_TYPE, "application/x-amz-json-1.1")
            .header("X-Amz-Target", target)
            .json(body)
            .send()
            .await
            .map_err(SchlageError::transport)?;
        let status = resp.status();
        let text = resp.text().await.map_err(SchlageError::transport)?;
        if !status.is_success() {
            return Err(classify_cognito_error(status.as_u16(), &text));
        }
        serde_json::from_str(&text)
            .map_err(|e| SchlageError::decode(format!("Cognito {target} response: {e}")))
    }
}

/// Classify a non-2xx Cognito IDP response body (`{"__type","message"}`). A
/// known auth-rejection `__type` becomes [`SchlageError::Credentials`]; any
/// other Cognito error keeps the raw status+body as [`SchlageError::Http`] so
/// it stays diagnosable.
fn classify_cognito_error(status: u16, body: &str) -> SchlageError {
    if let Some(t) = cognito_error_type(body) {
        // Cognito's `__type` may be bare (`NotAuthorizedException`) or prefixed
        // (`com.amazon...#NotAuthorizedException`); match on the suffix.
        if NOT_AUTHORIZED_TYPES.iter().any(|k| t.ends_with(k)) {
            return SchlageError::Credentials;
        }
    }
    SchlageError::from_http(status, body)
}

/// Extract Cognito's `__type` exception name from an error body, if present.
fn cognito_error_type(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("__type")
        .and_then(|t| t.as_str())
        .map(str::to_string)
}

/// Require the `PASSWORD_VERIFIER` challenge on an InitiateAuth response. A
/// different `ChallengeName` (MFA/new-password) is [`SchlageError::UnexpectedChallenge`];
/// a missing one is [`SchlageError::Decode`].
fn require_password_verifier(resp: &serde_json::Value) -> Result<(), SchlageError> {
    match resp.get("ChallengeName").and_then(|v| v.as_str()) {
        Some(PASSWORD_VERIFIER) => Ok(()),
        Some(other) => Err(SchlageError::UnexpectedChallenge(other.to_string())),
        None => Err(SchlageError::decode(
            "InitiateAuth response had no ChallengeName",
        )),
    }
}

/// After answering `PASSWORD_VERIFIER`, expect an `AuthenticationResult`. A
/// *further* `ChallengeName` is [`SchlageError::UnexpectedChallenge`]; neither
/// present is [`SchlageError::Decode`].
fn expect_auth_result(resp: &serde_json::Value) -> Result<&serde_json::Value, SchlageError> {
    if let Some(name) = resp.get("ChallengeName").and_then(|v| v.as_str()) {
        return Err(SchlageError::UnexpectedChallenge(name.to_string()));
    }
    resp.get("AuthenticationResult")
        .ok_or_else(|| SchlageError::decode("auth response had no AuthenticationResult"))
}

/// SECRET_HASH keyed on `user_id`, using this module's app-client constants.
fn secret_hash(user_id: &str) -> String {
    compute_secret_hash(CLIENT_SECRET, user_id, CLIENT_ID)
}

/// `base64(HMAC_SHA256(key = client_secret, message = user_id || client_id))` —
/// the AWS Cognito app-client secret hash. Split out so it can be tested against
/// an independently-computed known value.
fn compute_secret_hash(client_secret: &str, user_id: &str, client_id: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(client_secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(user_id.as_bytes());
    mac.update(client_id.as_bytes());
    BASE64.encode(mac.finalize().into_bytes())
}

/// A required string field of a Cognito JSON object; missing/wrong-typed is a
/// shape drift ([`SchlageError::Decode`]).
fn field_str<'a>(obj: &'a serde_json::Value, key: &str) -> Result<&'a str, SchlageError> {
    obj.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| SchlageError::decode(format!("response missing string field {key:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_hash_matches_known_value() {
        // Independently computed (arbitrary test values — NOT real credentials):
        //   python3 -c "import hmac,hashlib,base64; \
        //     print(base64.b64encode(hmac.new(b'test-client-secret', \
        //       b'user@example.com'+b'test-client-id', hashlib.sha256).digest()).decode())"
        //   -> JDFaz1Kl3Xp5KDXMm53WxP0U+ngLmtk3FN01nVGOnmQ=
        let got = compute_secret_hash("test-client-secret", "user@example.com", "test-client-id");
        assert_eq!(got, "JDFaz1Kl3Xp5KDXMm53WxP0U+ngLmtk3FN01nVGOnmQ=");
        // HMAC-SHA256 -> 32 bytes -> 44-char base64 (one '=' pad).
        assert_eq!(got.len(), 44);
    }

    #[test]
    fn cognito_auth_exception_is_credentials() {
        // The JSON-1.1 error shape Cognito returns for a bad login.
        let body =
            r#"{"__type":"NotAuthorizedException","message":"Incorrect username or password."}"#;
        assert!(matches!(
            classify_cognito_error(400, body),
            SchlageError::Credentials
        ));
        // Prefixed form is matched on the suffix too.
        let prefixed =
            r#"{"__type":"com.amazon.coral.service#UserNotFoundException","message":"x"}"#;
        assert!(matches!(
            classify_cognito_error(400, prefixed),
            SchlageError::Credentials
        ));
    }

    #[test]
    fn non_auth_cognito_error_stays_http() {
        let body = r#"{"__type":"InternalErrorException","message":"boom"}"#;
        let err = classify_cognito_error(500, body);
        assert!(matches!(err, SchlageError::Http { status: 500, .. }));
        assert_eq!(err.kind(), "http");
        // 5xx is transient, so run_schlage would `warn!` and retry.
        assert!(err.is_transient());
    }

    #[test]
    fn unexpected_challenge_is_classified() {
        // InitiateAuth that demands MFA instead of PASSWORD_VERIFIER.
        let mfa = json!({ "ChallengeName": "SMS_MFA", "ChallengeParameters": {} });
        assert!(matches!(
            require_password_verifier(&mfa),
            Err(SchlageError::UnexpectedChallenge(c)) if c == "SMS_MFA"
        ));
        // The expected challenge passes.
        let ok = json!({ "ChallengeName": "PASSWORD_VERIFIER", "ChallengeParameters": {} });
        assert!(require_password_verifier(&ok).is_ok());
        // No challenge name at all -> Decode (shape drift).
        let empty = json!({ "ChallengeParameters": {} });
        assert!(matches!(
            require_password_verifier(&empty),
            Err(SchlageError::Decode(_))
        ));
    }

    #[test]
    fn respond_to_challenge_further_challenge_is_unexpected() {
        // A second challenge after PASSWORD_VERIFIER (e.g. NEW_PASSWORD_REQUIRED).
        let further = json!({ "ChallengeName": "NEW_PASSWORD_REQUIRED" });
        assert!(matches!(
            expect_auth_result(&further),
            Err(SchlageError::UnexpectedChallenge(c)) if c == "NEW_PASSWORD_REQUIRED"
        ));
    }

    #[test]
    fn malformed_auth_result_is_decode() {
        // AuthenticationResult present but missing AccessToken -> Decode, the
        // loud "their API changed" signal (never a panic).
        let resp = json!({ "AuthenticationResult": { "IdToken": "x", "ExpiresIn": 3600 } });
        let auth = expect_auth_result(&resp).unwrap();
        let err = TokenState::from_auth_result(auth, None).unwrap_err();
        assert!(matches!(err, SchlageError::Decode(_)));
        assert_eq!(err.kind(), "decode");
    }

    #[test]
    fn refresh_reuses_prior_refresh_token_when_omitted() {
        // A refresh response omits RefreshToken; the prior one is carried over.
        let resp = json!({
            "AuthenticationResult": { "AccessToken": "new-access", "ExpiresIn": 3600 }
        });
        let auth = expect_auth_result(&resp).unwrap();
        let state = TokenState::from_auth_result(auth, Some("prior-refresh")).unwrap();
        assert_eq!(state.access_token, "new-access");
        assert_eq!(state.refresh_token, "prior-refresh");
    }

    #[test]
    fn field_str_reports_missing_key_as_decode() {
        let v = json!({ "present": "yes" });
        assert_eq!(field_str(&v, "present").unwrap(), "yes");
        assert!(matches!(
            field_str(&v, "absent"),
            Err(SchlageError::Decode(_))
        ));
    }
}
