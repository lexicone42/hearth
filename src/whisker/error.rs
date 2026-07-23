//! Typed, *diagnosable* errors for the Whisker (Litter-Robot 4) source.
//!
//! Same taxonomy as the Schlage source, so a user can tell apart:
//!   - [`WhiskerError::Credentials`] — "my Whisker password is wrong".
//!   - [`WhiskerError::UnexpectedChallenge`] — "my account needs attention" (MFA).
//!   - [`WhiskerError::Http`] — request rejected / endpoint moved / rate-limited
//!     / their cloud is down (status carried for interpretation).
//!   - [`WhiskerError::Decode`] — "their reverse-engineered API changed under
//!     us" (loud and distinct — includes GraphQL `errors` and a JWT/`mid` that
//!     no longer looks the way we expect).
//!   - [`WhiskerError::Transport`] — "the network hiccuped" (transient).
//!
//! Every fallible method in `auth`/`client`/`model` returns
//! `Result<_, WhiskerError>`; `main`'s `run_whisker` task inspects
//! [`WhiskerError::kind`] / [`WhiskerError::is_transient`] / [`WhiskerError::hint`]
//! to log the right level with an actionable message.

use thiserror::Error;

/// A Whisker source failure, classified so callers can log something actionable.
#[derive(Debug, Error)]
pub enum WhiskerError {
    /// Cognito rejected the login itself (bad password, unknown user, etc.).
    /// Persistent — the user must fix `[whisker]` credentials.
    #[error("Whisker credentials rejected by Cognito")]
    Credentials,

    /// Cognito returned an auth challenge we can't answer headlessly (MFA,
    /// NEW_PASSWORD_REQUIRED, SMS, ...). Persistent — the account needs attention.
    #[error("Cognito issued an unexpected challenge: {0}")]
    UnexpectedChallenge(String),

    /// A Whisker (or non-auth Cognito) HTTP call returned a non-2xx status.
    /// Transient for 429/5xx, persistent otherwise.
    #[error("Whisker API returned HTTP {status}: {body}")]
    Http {
        /// The HTTP status code.
        status: u16,
        /// The response body (captured for diagnosis).
        body: String,
    },

    /// A response (Cognito auth result / challenge, the id-token JWT, or the LR4
    /// GraphQL payload) did not match our structs — or the GraphQL response
    /// carried an `errors` array. This is the "their unofficial API changed under
    /// us" signal — surfaced loudly and distinctly, never a panic or a wrong value.
    #[error("Whisker response did not match the expected shape: {0}")]
    Decode(String),

    /// A network-level failure reaching Cognito or the Whisker API
    /// (connect/timeout/DNS/TLS). Transient.
    #[error("transport error reaching Whisker: {0}")]
    Transport(String),
}

impl WhiskerError {
    /// Build the error for a non-2xx Whisker/Cognito HTTP response. Factored out
    /// so the `status+body -> WhiskerError` seam is unit-testable.
    pub fn from_http(status: u16, body: impl Into<String>) -> Self {
        WhiskerError::Http {
            status,
            body: body.into(),
        }
    }

    /// A `reqwest` transport failure (`.send()` / body read).
    pub fn transport(e: impl std::fmt::Display) -> Self {
        WhiskerError::Transport(e.to_string())
    }

    /// A response shape mismatch — the loud "API changed" case.
    pub fn decode(context: impl std::fmt::Display) -> Self {
        WhiskerError::Decode(context.to_string())
    }

    /// A short, stable label for structured logs (`kind=...`).
    pub fn kind(&self) -> &'static str {
        match self {
            WhiskerError::Credentials => "credentials",
            WhiskerError::UnexpectedChallenge(_) => "unexpected_challenge",
            WhiskerError::Http { .. } => "http",
            WhiskerError::Decode(_) => "decode",
            WhiskerError::Transport(_) => "transport",
        }
    }

    /// Whether retrying on the next poll tick might succeed (transient) versus
    /// the problem needing human intervention (persistent). Drives `warn!` vs
    /// `error!` at the task boundary.
    pub fn is_transient(&self) -> bool {
        match self {
            WhiskerError::Transport(_) => true,
            // Rate-limit and server-side errors clear on their own; other 4xx
            // (401/403/404) reflect a durable problem (token/endpoint).
            WhiskerError::Http { status, .. } => *status == 429 || *status >= 500,
            WhiskerError::Credentials
            | WhiskerError::UnexpectedChallenge(_)
            | WhiskerError::Decode(_) => false,
        }
    }

    /// A human, actionable hint for the log line.
    pub fn hint(&self) -> String {
        match self {
            WhiskerError::Credentials => {
                "Whisker auth rejected — check [whisker] username/password".into()
            }
            WhiskerError::UnexpectedChallenge(c) => format!(
                "Whisker account needs attention (Cognito challenge {c}) — cannot proceed headless"
            ),
            WhiskerError::Http { status, .. } => match status {
                401 | 403 => format!(
                    "Whisker API rejected the request ({status}) — the id token was not accepted"
                ),
                404 => "Whisker API returned 404 — the endpoint may have moved".into(),
                429 => "Whisker API rate-limited (429) — will retry next tick".into(),
                s if *s >= 500 => {
                    format!("Whisker cloud error ({status}) — likely their side, will retry")
                }
                _ => format!("Whisker API returned HTTP {status}"),
            },
            WhiskerError::Decode(_) => {
                "Whisker API response didn't match — their (unofficial) cloud API may have changed"
                    .into()
            }
            WhiskerError::Transport(_) => {
                "Whisker/Cognito unreachable (network/DNS/TLS) — will retry next tick".into()
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
            let e = WhiskerError::from_http(status, "nope");
            assert!(matches!(e, WhiskerError::Http { status: s, .. } if s == status));
            assert_eq!(e.kind(), "http");
            assert!(!e.is_transient(), "{status} should be persistent");
        }
        for status in [429u16, 500, 503] {
            let e = WhiskerError::from_http(status, "later");
            assert!(matches!(e, WhiskerError::Http { status: s, .. } if s == status));
            assert!(e.is_transient(), "{status} should be transient");
        }
    }

    #[test]
    fn hints_are_status_specific() {
        assert!(WhiskerError::from_http(401, "").hint().contains("id token"));
        assert!(WhiskerError::from_http(404, "").hint().contains("moved"));
        assert!(WhiskerError::from_http(429, "").hint().contains("rate"));
        assert!(WhiskerError::from_http(502, "").hint().contains("cloud"));
    }

    #[test]
    fn kinds_and_transience_of_the_non_http_variants() {
        assert_eq!(WhiskerError::Credentials.kind(), "credentials");
        assert!(!WhiskerError::Credentials.is_transient());

        let ch = WhiskerError::UnexpectedChallenge("SMS_MFA".into());
        assert_eq!(ch.kind(), "unexpected_challenge");
        assert!(!ch.is_transient());

        let dec = WhiskerError::decode("field x");
        assert_eq!(dec.kind(), "decode");
        assert!(!dec.is_transient());
        // The decode hint must clearly call out an API change.
        assert!(dec.hint().contains("may have changed"));

        let tr = WhiskerError::transport("dns");
        assert_eq!(tr.kind(), "transport");
        assert!(tr.is_transient());
    }
}
