//! Dyson source: subscribes to a local Dyson purifier/fan over MQTT and
//! normalizes its state/sensor messages into canonical
//! [`crate::domain::Observation`]s, feeding the SAME internal event bus the
//! poll sources use. Unlike `ambient`/`ecoflow` it is PUSH, not poll: it has no
//! tick and emits an observation batch on every incoming MQTT publish — which is
//! exactly what the Phase-1 event bus exists to enable.
//!
//! Module shape mirrors the other sources:
//!   - [`credential`] derives the MQTT identity (serial / product type /
//!     password) fully locally from the device's setup SSID + Wi-Fi password,
//!     reproducing libdyson's `get_mqtt_info_from_wifi_info` (NO cloud);
//!   - [`model`]      parses inbound `status/current` messages into a flat field
//!     map keyed by message type;
//!   - [`canonical`]  maps that field map to the domain (`dyson.<serial>.<channel>`);
//!   - [`source`]     owns the long-lived `rumqttc` task (connect, subscribe,
//!     prime, auto-reconnect).
//!
//! The whole source is a no-op when the `[dyson]` config section is absent —
//! `main` only builds a [`source::DysonSource`] when the section is present, and
//! any build/connection error is logged, never fatal (matching EcoFlow).
//!
//! Protocol/credential/field-decode details were verified against
//! `shenxn/libdyson` (`libdyson/utils.py`, `dyson_device.py`,
//! `dyson_pure_cool.py`, `const.py`), the maintained fork
//! `libdyson-wg/libdyson-neon`, and `libdyson-wg/ha-dyson`; see the per-module
//! doc comments for the specific code paths confirmed.
//!
//! TODO(live-device): two things need a real device + sticker to confirm:
//!   1. MQTT protocol version — libdyson uses MQTTv31; rumqttc 0.24 speaks
//!      3.1.1 and exposes no selector. The broker is expected to accept 3.1.1
//!      (as ha-dyson relies on), but verify against the live unit.
//!   2. Product type — the target is a Pure Cool family unit (`438`, per the
//!      `const.py` map); confirm the sticker's numeric suffix matches.
pub mod canonical;
pub mod credential;
pub mod model;
pub mod source;
