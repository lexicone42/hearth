//! Schlage source: polls a Schlage Wi-Fi lock's state from Schlage's
//! (unofficial, reverse-engineered) cloud and normalizes it into canonical
//! [`crate::domain::Observation`]s so it reaches the local API sink / watch tile.
//!
//! Mirrors the `ecoflow` module's shape:
//!   - [`auth`]      performs the AWS Cognito SRP handshake (and cheap refresh),
//!   - [`client`]    fetches over HTTPS with the `X-Api-Key` + `Bearer` token,
//!   - [`model`]     deserializes the device JSON,
//!   - [`canonical`] maps a device to lock/battery observations.
//!
//! The whole source is a no-op when the `[schlage]` config section is absent —
//! `main` only constructs a [`client::SchlageClient`] when credentials exist.
//!
//! This talks to Schlage's private cloud (`api.allegion.yonomi.cloud`), the same
//! endpoints the Schlage Home app uses. The constants (Cognito pool/client,
//! API key, base URL) are verified against `dknowles2/pyschlage`. Being an
//! unofficial API, it can break whenever Schlage changes it — every error here
//! is logged, never fatal.
pub mod auth;
pub mod canonical;
pub mod client;
pub mod error;
pub mod model;

pub use error::SchlageError;
