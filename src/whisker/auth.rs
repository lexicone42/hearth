//! AWS Cognito SRP authentication for the Whisker (Litter-Robot) cloud.
//!
//! Whisker's app authenticates against an AWS Cognito user pool using the Secure
//! Remote Password (SRP) protocol, then carries the resulting Cognito **id
//! token** as a `Bearer` credential on every LR4 GraphQL request.
//!
//! We use [`aws_cognito_srp`] to do the SRP math (it generates the `SRP_A` /
//! password-claim parameters) but make the two Cognito IDP calls ourselves as
//! raw `x-amz-json-1.1` POSTs with `reqwest` — keeping deps lean (no
//! `aws-sdk-cognitoidentityprovider`).
//!
//! ## Differences from the Schlage Cognito flow (verified against
//! `natekspencer/pylitterbot` `session.py`)
//!
//! This Whisker app client has **NO client secret**, so — unlike Schlage — there
//! is **no `SECRET_HASH` anywhere**: not on `InitiateAuth`, not on
//! `RespondToAuthChallenge`, not on `REFRESH_TOKEN_AUTH`. The [`SrpClient`] is
//! constructed with `None` for the secret.
//!
//! ## Flow
//!
//! 1. **InitiateAuth** (`USER_SRP_AUTH`): send `USERNAME` and `SRP_A`. Cognito
//!    replies with the `PASSWORD_VERIFIER` challenge (`SRP_B`, `SALT`,
//!    `SECRET_BLOCK`, `USER_ID_FOR_SRP`).
//! 2. **RespondToAuthChallenge** (`PASSWORD_VERIFIER`): send the computed
//!    `PASSWORD_CLAIM_SIGNATURE`, `PASSWORD_CLAIM_SECRET_BLOCK`, `TIMESTAMP`, and
//!    the challenge's `USER_ID_FOR_SRP` as `USERNAME`. Cognito replies with
//!    `IdToken` / `AccessToken` / `RefreshToken` / `ExpiresIn`.
//!
//! The **id token** is what goes in the LR4 `Authorization` header as
//! `Bearer <id_token>` (pylitterbot's `Session.get_bearer_authorization` uses
//! `async_get_id_token`), and the user id used to discover robots is the `mid`
//! claim decoded from that same id token (pylitterbot's `get_user_id`).
//!
//! Tokens are held in memory only (no persistence — re-auth is cheap and
//! non-interactive). On expiry we try a cheap `REFRESH_TOKEN_AUTH`; if that
//! fails, we fall back to a full SRP handshake.

use aws_cognito_srp::{SrpClient, User, UserAuthenticationParameters, VerificationParameters};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::whisker::WhiskerError;

// ---- Constants (verified against pylitterbot `session.py`, base64-decoded) ----
// USER_POOL_ID  b64 "dXMtZWFzdC0xX3JqaE5uWlZBbQ==" -> "us-east-1_rjhNnZVAm"
// CLIENT_ID     b64 "NDU1MnVqZXUzYWljOTBuZjhxbjUzbGV2bW4=" -> "4552ujeu3aic90nf8qn53levmn"
// There is NO client secret for this app client, so no SECRET_HASH is ever sent.
const USER_POOL_ID: &str = "us-east-1_rjhNnZVAm";
const CLIENT_ID: &str = "4552ujeu3aic90nf8qn53levmn";

/// The regional Cognito IDP endpoint (`us-east-1`). All IDP actions POST here.
const COGNITO_IDP_URL: &str = "https://cognito-idp.us-east-1.amazonaws.com/";
const TARGET_INITIATE_AUTH: &str = "AWSCognitoIdentityProviderService.InitiateAuth";
const TARGET_RESPOND_TO_CHALLENGE: &str =
    "AWSCognitoIdentityProviderService.RespondToAuthChallenge";

/// The one SRP challenge we can answer headlessly.
const PASSWORD_VERIFIER: &str = "PASSWORD_VERIFIER";

/// Cognito `__type` exception suffixes that mean the login itself was rejected
/// (the user must fix their credentials).
const NOT_AUTHORIZED_TYPES: &[&str] = &[
    "NotAuthorizedException",
    "InvalidPasswordException",
    "PasswordResetRequiredException",
    "UserNotFoundException",
    "UserNotConfirmedException",
];

/// Refresh this many seconds *before* the id token actually expires, so an
/// in-flight request never carries a just-expired token.
const EXPIRY_MARGIN_SECS: i64 = 60;

/// Cognito SRP authenticator: owns the credentials and the current token state,
/// and hands out a valid **id token** on demand (refreshing / re-authing as
/// needed). `token()` takes `&self`; the token state lives behind a `Mutex` so
/// a single refresh is serialized rather than raced.
pub struct CognitoAuth {
    http: reqwest::Client,
    username: String,
    password: String,
    state: Mutex<Option<TokenState>>,
}

/// The in-memory token cache: the id token used for the LR4 API (as a bearer),
/// the refresh token to renew it cheaply, and when to consider it stale.
#[derive(Debug)]
struct TokenState {
    id_token: String,
    refresh_token: String,
    expires_at: Instant,
}

impl TokenState {
    /// Build from a Cognito `AuthenticationResult` object. Refresh responses
    /// omit `RefreshToken`, so `fallback_refresh` (the prior one) is reused then.
    /// A missing `IdToken` is a [`WhiskerError::Decode`] (shape drift) — Whisker
    /// uses the **id** token, not the access token, as its bearer.
    fn from_auth_result(
        auth: &serde_json::Value,
        fallback_refresh: Option<&str>,
    ) -> Result<Self, WhiskerError> {
        let id_token = field_str(auth, "IdToken")?.to_string();
        let refresh_token = match auth.get("RefreshToken").and_then(|v| v.as_str()) {
            Some(rt) => rt.to_string(),
            None => fallback_refresh
                .ok_or_else(|| {
                    WhiskerError::decode("auth result has no RefreshToken and no prior token")
                })?
                .to_string(),
        };
        // Cognito id tokens default to 3600s; trust ExpiresIn when present.
        let expires_in = auth
            .get("ExpiresIn")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(3600);
        let ttl = expires_in.saturating_sub(EXPIRY_MARGIN_SECS).max(0) as u64;
        Ok(Self {
            id_token,
            refresh_token,
            expires_at: Instant::now() + Duration::from_secs(ttl),
        })
    }
}

impl CognitoAuth {
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, WhiskerError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(WhiskerError::transport)?;
        Ok(Self {
            http,
            username: username.into(),
            password: password.into(),
            state: Mutex::new(None),
        })
    }

    /// A currently-valid Cognito **id token** (the LR4 bearer). Returns the
    /// cached token when fresh; otherwise refreshes (cheap) and, if that fails,
    /// does a full SRP re-auth. Errors are typed ([`WhiskerError`]).
    pub async fn token(&self) -> Result<String, WhiskerError> {
        let mut guard = self.state.lock().await;

        if let Some(state) = guard.as_ref() {
            if Instant::now() < state.expires_at {
                return Ok(state.id_token.clone());
            }
            // Stale: try a cheap refresh first.
            let refresh_token = state.refresh_token.clone();
            debug!("Whisker: refreshing Cognito token");
            match self.refresh(&refresh_token).await {
                Ok(new_state) => {
                    let token = new_state.id_token.clone();
                    *guard = Some(new_state);
                    debug!("Whisker: token refreshed");
                    return Ok(token);
                }
                Err(e) => {
                    warn!(kind = e.kind(), error = %e, "Whisker: token refresh failed; re-authenticating")
                }
            }
        }

        debug!("Whisker: authenticating (Cognito SRP)");
        let new_state = self.full_authenticate().await?;
        let token = new_state.id_token.clone();
        *guard = Some(new_state);
        debug!("Whisker: authenticated");
        Ok(token)
    }

    /// The full `USER_SRP_AUTH` -> `PASSWORD_VERIFIER` handshake. No SECRET_HASH
    /// anywhere: this Cognito app client has no secret.
    async fn full_authenticate(&self) -> Result<TokenState, WhiskerError> {
        // No client secret -> pass `None`; nothing computes a SECRET_HASH.
        let srp = SrpClient::new(
            User::new(USER_POOL_ID, &self.username, &self.password),
            CLIENT_ID,
            None,
        );
        let UserAuthenticationParameters { a, username } = srp.get_auth_parameters();

        // Part 1: InitiateAuth (USER_SRP_AUTH). USERNAME + SRP_A only.
        let init = self
            .cognito_idp(
                TARGET_INITIATE_AUTH,
                &json!({
                    "AuthFlow": "USER_SRP_AUTH",
                    "ClientId": CLIENT_ID,
                    "AuthParameters": {
                        "USERNAME": username,
                        "SRP_A": a,
                    },
                }),
            )
            .await?;

        // Cognito should answer with the PASSWORD_VERIFIER challenge; anything
        // else (MFA/new-password) we can't answer headlessly.
        require_password_verifier(&init)?;
        let params = init.get("ChallengeParameters").ok_or_else(|| {
            WhiskerError::decode("InitiateAuth response had no ChallengeParameters")
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
                WhiskerError::decode(format!(
                    "SRP verify failed (bad Cognito challenge params?): {e}"
                ))
            })?;

        // Part 2: RespondToAuthChallenge (PASSWORD_VERIFIER). USERNAME is now the
        // challenge's USER_ID_FOR_SRP. Still no SECRET_HASH.
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
                    },
                }),
            )
            .await?;

        // A successful verify yields AuthenticationResult; a *further* challenge
        // (SMS_MFA, NEW_PASSWORD_REQUIRED, ...) is UnexpectedChallenge.
        let auth = expect_auth_result(&resp)?;
        TokenState::from_auth_result(auth, None)
    }

    /// Cheap token renewal via `REFRESH_TOKEN_AUTH`. No SECRET_HASH (no secret).
    async fn refresh(&self, refresh_token: &str) -> Result<TokenState, WhiskerError> {
        let resp = self
            .cognito_idp(
                TARGET_INITIATE_AUTH,
                &json!({
                    "AuthFlow": "REFRESH_TOKEN_AUTH",
                    "ClientId": CLIENT_ID,
                    "AuthParameters": {
                        "REFRESH_TOKEN": refresh_token,
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
    ) -> Result<serde_json::Value, WhiskerError> {
        let resp = self
            .http
            .post(COGNITO_IDP_URL)
            .header(reqwest::header::CONTENT_TYPE, "application/x-amz-json-1.1")
            .header("X-Amz-Target", target)
            .json(body)
            .send()
            .await
            .map_err(WhiskerError::transport)?;
        let status = resp.status();
        let text = resp.text().await.map_err(WhiskerError::transport)?;
        if !status.is_success() {
            return Err(classify_cognito_error(status.as_u16(), &text));
        }
        serde_json::from_str(&text)
            .map_err(|e| WhiskerError::decode(format!("Cognito {target} response: {e}")))
    }
}

/// Decode the `mid` claim (the Whisker user id) from a Cognito **id token**.
///
/// A JWT is `header.payload.signature`; the middle segment is base64url
/// (unpadded) JSON. pylitterbot's `get_user_id` does exactly this — it decodes
/// the id token and reads the `mid` claim. We do NOT verify the signature (the
/// token came straight from our own Cognito exchange over TLS); we only read a
/// claim from it. A malformed token / missing claim is [`WhiskerError::Decode`].
pub(crate) fn mid_from_id_token(id_token: &str) -> Result<String, WhiskerError> {
    let payload_b64 = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| WhiskerError::decode("id token is not a JWT (no payload segment)"))?;
    // Tolerate any stray '=' padding, though JWTs are unpadded base64url.
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_b64.trim_end_matches('='))
        .map_err(|e| WhiskerError::decode(format!("id token payload not base64url: {e}")))?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| WhiskerError::decode(format!("id token payload not JSON: {e}")))?;
    claims
        .get("mid")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| WhiskerError::decode("id token has no `mid` claim"))
}

/// Classify a non-2xx Cognito IDP response body (`{"__type","message"}`). A
/// known auth-rejection `__type` becomes [`WhiskerError::Credentials`]; any other
/// Cognito error keeps the raw status+body as [`WhiskerError::Http`].
fn classify_cognito_error(status: u16, body: &str) -> WhiskerError {
    if let Some(t) = cognito_error_type(body) {
        // Cognito's `__type` may be bare (`NotAuthorizedException`) or prefixed
        // (`com.amazon...#NotAuthorizedException`); match on the suffix.
        if NOT_AUTHORIZED_TYPES.iter().any(|k| t.ends_with(k)) {
            return WhiskerError::Credentials;
        }
    }
    WhiskerError::from_http(status, body)
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
/// different `ChallengeName` (MFA/new-password) is [`WhiskerError::UnexpectedChallenge`];
/// a missing one is [`WhiskerError::Decode`].
fn require_password_verifier(resp: &serde_json::Value) -> Result<(), WhiskerError> {
    match resp.get("ChallengeName").and_then(|v| v.as_str()) {
        Some(PASSWORD_VERIFIER) => Ok(()),
        Some(other) => Err(WhiskerError::UnexpectedChallenge(other.to_string())),
        None => Err(WhiskerError::decode(
            "InitiateAuth response had no ChallengeName",
        )),
    }
}

/// After answering `PASSWORD_VERIFIER`, expect an `AuthenticationResult`. A
/// *further* `ChallengeName` is [`WhiskerError::UnexpectedChallenge`]; neither
/// present is [`WhiskerError::Decode`].
fn expect_auth_result(resp: &serde_json::Value) -> Result<&serde_json::Value, WhiskerError> {
    if let Some(name) = resp.get("ChallengeName").and_then(|v| v.as_str()) {
        return Err(WhiskerError::UnexpectedChallenge(name.to_string()));
    }
    resp.get("AuthenticationResult")
        .ok_or_else(|| WhiskerError::decode("auth response had no AuthenticationResult"))
}

/// A required string field of a Cognito JSON object; missing/wrong-typed is a
/// shape drift ([`WhiskerError::Decode`]).
fn field_str<'a>(obj: &'a serde_json::Value, key: &str) -> Result<&'a str, WhiskerError> {
    obj.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| WhiskerError::decode(format!("response missing string field {key:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an unsigned JWT with the given JSON payload (header/signature are
    /// arbitrary — we never verify them). No real credentials involved.
    fn fake_jwt(payload_json: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        format!("{header}.{payload}.signature-not-checked")
    }

    #[test]
    fn mid_claim_is_decoded_from_the_id_token() {
        let jwt = fake_jwt(r#"{"sub":"abc","mid":"user-mid-42","email":"x@example.com"}"#);
        assert_eq!(mid_from_id_token(&jwt).unwrap(), "user-mid-42");
    }

    #[test]
    fn id_token_without_mid_is_decode() {
        let jwt = fake_jwt(r#"{"sub":"abc","email":"x@example.com"}"#);
        let err = mid_from_id_token(&jwt).unwrap_err();
        assert_eq!(err.kind(), "decode");
    }

    #[test]
    fn non_jwt_id_token_is_decode() {
        // No '.' segments -> not a JWT.
        let err = mid_from_id_token("not-a-jwt").unwrap_err();
        assert_eq!(err.kind(), "decode");
        // Payload segment that isn't base64url -> Decode, never a panic.
        let err = mid_from_id_token("aaa.$$$notbase64$$$.bbb").unwrap_err();
        assert_eq!(err.kind(), "decode");
    }

    #[test]
    fn cognito_auth_exception_is_credentials() {
        // The JSON-1.1 error shape Cognito returns for a bad login.
        let body =
            r#"{"__type":"NotAuthorizedException","message":"Incorrect username or password."}"#;
        assert!(matches!(
            classify_cognito_error(400, body),
            WhiskerError::Credentials
        ));
        // A live probe of this exact pool returns UserNotFoundException for an
        // unknown user — also a credentials problem. Prefixed form matched too.
        let prefixed =
            r#"{"__type":"com.amazon.coral.service#UserNotFoundException","message":"x"}"#;
        assert!(matches!(
            classify_cognito_error(400, prefixed),
            WhiskerError::Credentials
        ));
    }

    #[test]
    fn non_auth_cognito_error_stays_http() {
        let body = r#"{"__type":"InternalErrorException","message":"boom"}"#;
        let err = classify_cognito_error(500, body);
        assert!(matches!(err, WhiskerError::Http { status: 500, .. }));
        assert_eq!(err.kind(), "http");
        assert!(err.is_transient());
    }

    #[test]
    fn unexpected_challenge_is_classified() {
        let mfa = json!({ "ChallengeName": "SMS_MFA", "ChallengeParameters": {} });
        assert!(matches!(
            require_password_verifier(&mfa),
            Err(WhiskerError::UnexpectedChallenge(c)) if c == "SMS_MFA"
        ));
        let ok = json!({ "ChallengeName": "PASSWORD_VERIFIER", "ChallengeParameters": {} });
        assert!(require_password_verifier(&ok).is_ok());
        let empty = json!({ "ChallengeParameters": {} });
        assert!(matches!(
            require_password_verifier(&empty),
            Err(WhiskerError::Decode(_))
        ));
    }

    #[test]
    fn respond_to_challenge_further_challenge_is_unexpected() {
        let further = json!({ "ChallengeName": "NEW_PASSWORD_REQUIRED" });
        assert!(matches!(
            expect_auth_result(&further),
            Err(WhiskerError::UnexpectedChallenge(c)) if c == "NEW_PASSWORD_REQUIRED"
        ));
    }

    #[test]
    fn auth_result_uses_id_token_not_access_token() {
        // Whisker's bearer is the IdToken. An AuthenticationResult with only an
        // AccessToken (no IdToken) is Decode, the loud "API changed" signal.
        let resp = json!({
            "AuthenticationResult": { "AccessToken": "acc", "ExpiresIn": 3600 }
        });
        let auth = expect_auth_result(&resp).unwrap();
        let err = TokenState::from_auth_result(auth, None).unwrap_err();
        assert!(matches!(err, WhiskerError::Decode(_)));

        // With an IdToken present it parses, and that's the token we carry.
        let resp = json!({
            "AuthenticationResult": {
                "IdToken": "id-tok", "AccessToken": "acc",
                "RefreshToken": "ref", "ExpiresIn": 3600
            }
        });
        let auth = expect_auth_result(&resp).unwrap();
        let state = TokenState::from_auth_result(auth, None).unwrap();
        assert_eq!(state.id_token, "id-tok");
        assert_eq!(state.refresh_token, "ref");
    }

    #[test]
    fn refresh_reuses_prior_refresh_token_when_omitted() {
        // A refresh response omits RefreshToken; the prior one is carried over.
        let resp = json!({
            "AuthenticationResult": { "IdToken": "new-id", "ExpiresIn": 3600 }
        });
        let auth = expect_auth_result(&resp).unwrap();
        let state = TokenState::from_auth_result(auth, Some("prior-refresh")).unwrap();
        assert_eq!(state.id_token, "new-id");
        assert_eq!(state.refresh_token, "prior-refresh");
    }

    #[test]
    fn field_str_reports_missing_key_as_decode() {
        let v = json!({ "present": "yes" });
        assert_eq!(field_str(&v, "present").unwrap(), "yes");
        assert!(matches!(
            field_str(&v, "absent"),
            Err(WhiskerError::Decode(_))
        ));
    }
}
