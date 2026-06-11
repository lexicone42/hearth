//! EcoFlow source: polls the EcoFlow IoT Open API and normalizes a device's
//! "quota" (its full live state map) into canonical [`crate::domain::Observation`]s.
//!
//! Mirrors the `ambient` module's shape exactly:
//!   - [`client`]   fetches over HTTPS (with EcoFlow's HMAC-SHA256 request signing),
//!   - [`model`]    deserializes the `{code, message, data}` envelopes,
//!   - [`canonical`] maps the flat `"property.path" -> scalar` quota map to the domain.
//!
//! The whole source is a no-op when the `[ecoflow]` config section is absent —
//! `main` only constructs an [`client::EcoflowClient`] when credentials exist.
pub mod canonical;
pub mod client;
pub mod model;
