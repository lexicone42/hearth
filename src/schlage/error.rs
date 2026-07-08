//! Typed, *diagnosable* errors for the Schlage source.
//!
//! The whole point of this taxonomy is to let a user tell apart:
//!   - [`SchlageError::Credentials`] — "my password is wrong".
//!   - [`SchlageError::UnexpectedChallenge`] — "my account needs attention" (MFA).
//!   - [`SchlageError::Http`] — request rejected / endpoint moved / rate-limited
//!     / their cloud is down (status carried for interpretation).
//!   - [`SchlageError::Decode`] — "their reverse-engineered API changed under
//!     us" (loud and distinct).
//!   - [`SchlageError::Transport`] — "the network hiccuped" (transient).
//!
//! Every fallible method in `auth`/`client` returns `Result<_, SchlageError>`;
//! `main`'s `run_schlage` task inspects [`SchlageError::kind`] /
//! [`SchlageError::is_transient`] / [`SchlageError::hint`] to log the right
//! level with an actionable message, and construction errors map into `anyhow`
//! at the task boundary.

use thiserror::Error;

/// A Schlage source failure, classified so callers can log something actionable.
#[derive(Debug, Error)]
pub enum SchlageError {
    /// Cognito rejected the login itself (bad password, unknown user, etc.).
    /// Persistent — the user must fix `[schlage]` credentials.
    #[error("Schlage credentials rejected by Cognito")]
    Credentials,

    /// Cognito returned an auth challenge we can't answer headlessly (MFA,
    /// NEW_PASSWORD_REQUIRED, SMS, ...). Persistent — the account needs attention.
    #[error("Cognito issued an unexpected challenge: {0}")]
    UnexpectedChallenge(String),

    /// A Schlage (or non-auth Cognito) HTTP call returned a non-2xx status.
    /// Transient for 429/5xx, persistent otherwise.
    #[error("Schlage API returned HTTP {status}: {body}")]
    Http {
        /// The HTTP status code.
        status: u16,
        /// The response body (captured for diagnosis).
        body: String,
    },

    /// A response (Cognito auth result / challenge, or the device JSON) did not
    /// match our structs. This is the "their unofficial API changed under us"
    /// signal — surfaced loudly and distinctly, never a panic or a wrong value.
    #[error("Schlage response did not match the expected shape: {0}")]
    Decode(String),

    /// A network-level failure reaching Cognito or the Schlage API
    /// (connect/timeout/DNS/TLS). Transient.
    #[error("transport error reaching Schlage: {0}")]
    Transport(String),
}

impl SchlageError {
    /// Build the error for a non-2xx Schlage/Cognito HTTP response. Factored out
    /// so the `status+body -> SchlageError` seam is unit-testable.
    pub fn from_http(status: u16, body: impl Into<String>) -> Self {
        SchlageError::Http {
            status,
            body: body.into(),
        }
    }

    /// A `reqwest` transport failure (`.send()` / body read).
    pub fn transport(e: impl std::fmt::Display) -> Self {
        SchlageError::Transport(e.to_string())
    }

    /// A response shape mismatch — the loud "API changed" case.
    pub fn decode(context: impl std::fmt::Display) -> Self {
        SchlageError::Decode(context.to_string())
    }

    /// A short, stable label for structured logs (`kind=...`).
    pub fn kind(&self) -> &'static str {
        match self {
            SchlageError::Credentials => "credentials",
            SchlageError::UnexpectedChallenge(_) => "unexpected_challenge",
            SchlageError::Http { .. } => "http",
            SchlageError::Decode(_) => "decode",
            SchlageError::Transport(_) => "transport",
        }
    }

    /// Whether retrying on the next poll tick might succeed (transient) versus
    /// the problem needing human intervention (persistent). Drives `warn!` vs
    /// `error!` at the task boundary.
    pub fn is_transient(&self) -> bool {
        match self {
            SchlageError::Transport(_) => true,
            // Rate-limit and server-side errors clear on their own; other 4xx
            // (401/403/404) reflect a durable problem (token/key/endpoint).
            SchlageError::Http { status, .. } => *status == 429 || *status >= 500,
            SchlageError::Credentials
            | SchlageError::UnexpectedChallenge(_)
            | SchlageError::Decode(_) => false,
        }
    }

    /// A human, actionable hint for the log line.
    pub fn hint(&self) -> String {
        match self {
            SchlageError::Credentials => {
                "Schlage auth rejected — check [schlage] username/password".into()
            }
            SchlageError::UnexpectedChallenge(c) => format!(
                "Schlage account needs attention (Cognito challenge {c}) — cannot proceed headless"
            ),
            SchlageError::Http { status, .. } => match status {
                401 | 403 => format!(
                    "Schlage API rejected the request ({status}) — access token or X-Api-Key not accepted"
                ),
                404 => "Schlage API returned 404 — the endpoint may have moved".into(),
                429 => "Schlage API rate-limited (429) — will retry next tick".into(),
                s if *s >= 500 => {
                    format!("Schlage cloud error ({status}) — likely their side, will retry")
                }
                _ => format!("Schlage API returned HTTP {status}"),
            },
            SchlageError::Decode(_) => {
                "Schlage API response didn't match — their (unofficial) cloud API may have changed"
                    .into()
            }
            SchlageError::Transport(_) => {
                "Schlage/Cognito unreachable (network/DNS/TLS) — will retry next tick".into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_maps_to_http_variant_with_transience() {
        for status in [401u16, 403, 404] {
            let e = SchlageError::from_http(status, "nope");
            assert!(matches!(e, SchlageError::Http { status: s, .. } if s == status));
            assert_eq!(e.kind(), "http");
            assert!(!e.is_transient(), "{status} should be persistent");
        }
        for status in [429u16, 500, 503] {
            let e = SchlageError::from_http(status, "later");
            assert!(matches!(e, SchlageError::Http { status: s, .. } if s == status));
            assert!(e.is_transient(), "{status} should be transient");
        }
    }

    #[test]
    fn hints_are_status_specific() {
        assert!(SchlageError::from_http(401, "").hint().contains("token"));
        assert!(SchlageError::from_http(404, "").hint().contains("moved"));
        assert!(SchlageError::from_http(429, "").hint().contains("rate"));
        assert!(SchlageError::from_http(502, "").hint().contains("cloud"));
    }

    #[test]
    fn kinds_and_transience_of_the_non_http_variants() {
        assert_eq!(SchlageError::Credentials.kind(), "credentials");
        assert!(!SchlageError::Credentials.is_transient());

        let ch = SchlageError::UnexpectedChallenge("SMS_MFA".into());
        assert_eq!(ch.kind(), "unexpected_challenge");
        assert!(!ch.is_transient());

        let dec = SchlageError::decode("field x");
        assert_eq!(dec.kind(), "decode");
        assert!(!dec.is_transient());
        // The decode hint must clearly call out an API change.
        assert!(dec.hint().contains("may have changed"));

        let tr = SchlageError::transport("dns");
        assert_eq!(tr.kind(), "transport");
        assert!(tr.is_transient());
    }
}
