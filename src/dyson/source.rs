//! Dyson MQTT push-source: a long-lived task that subscribes to a local Dyson
//! purifier/fan's `status/current` topic and emits canonical observations onto
//! the event bus on every incoming message. No tick — it produces on push,
//! which is the whole point of the Phase-1 event bus.
//!
//! Protocol verified against libdyson (`shenxn/libdyson` + `libdyson-wg/ha-dyson`):
//!
//! * connect to the device over plaintext MQTT on `:1883` (TLS optional, for
//!   future `:8883` models); `username = serial`, `password = credential`,
//!   keepalive ~60s (`dyson_device.py` `username_pw_set` + paho default);
//! * subscribe to `{product_type}/{serial}/status/current`
//!   (and `.../status/faults`, best-effort);
//! * on each fresh connection, PRIME state by publishing to
//!   `{product_type}/{serial}/command` the two messages
//!   `{"msg":"REQUEST-CURRENT-STATE","time":<iso8601-utc-Z>}` and
//!   `{"msg":"REQUEST-PRODUCT-ENVIRONMENT-CURRENT-SENSOR-DATA","time":...}`
//!   (`mqtt_time()` = `strftime("%Y-%m-%dT%H:%M:%SZ", gmtime())`);
//! * drive the EventLoop forever, AUTO-RECONNECTING on poll errors — re-prime
//!   on each new ConnAck (rumqttc re-sends our stored subscriptions itself).
//!
//! NOTE on MQTT protocol version: libdyson connects with MQTTv31; rumqttc 0.24
//! speaks MQTT 3.1.1 and exposes no protocol selector. Dyson's broker accepts
//! 3.1.1 in practice (this is what ha-dyson and community clients rely on), so
//! we connect with 3.1.1. See the TODO in `mod.rs` — confirm against the live
//! device once real sticker values are available.

use std::time::Duration;

use anyhow::{Context, Result};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, Transport};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::DysonConfig;
use crate::domain::{Observation, UnitSystem};
use crate::dyson::credential::{self, MqttInfo};
use crate::dyson::{canonical, model};

/// MQTT keepalive, matching paho's default (and what libdyson relies on).
const KEEPALIVE_SECS: u64 = 60;

/// Bounded backoff between reconnect attempts after an EventLoop poll error.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// A configured-but-not-yet-connected Dyson source.
pub struct DysonSource {
    host: String,
    port: u16,
    tls: bool,
    info: MqttInfo,
}

impl DysonSource {
    /// Resolve the MQTT identity from config: explicit `serial`/`product_type`/
    /// `credential` overrides win; otherwise derive everything locally from the
    /// setup SSID + Wi-Fi password (libdyson `get_mqtt_info_from_wifi_info`).
    pub fn from_config(cfg: &DysonConfig) -> Result<Self> {
        let info = resolve_mqtt_info(cfg)?;
        info!(
            host = %cfg.host,
            port = cfg.port,
            tls = cfg.tls,
            serial = %info.serial,
            product_type = %info.product_type,
            "configured Dyson source"
        );
        Ok(Self {
            host: cfg.host.clone(),
            port: cfg.port,
            tls: cfg.tls,
            info,
        })
    }

    /// `{product_type}/{serial}/status/current` — the state/sensor feed.
    fn status_topic(&self) -> String {
        format!("{}/{}/status/current", self.info.product_type, self.info.serial)
    }

    /// `{product_type}/{serial}/status/faults` — optional fault feed.
    fn faults_topic(&self) -> String {
        format!("{}/{}/status/faults", self.info.product_type, self.info.serial)
    }

    /// `{product_type}/{serial}/command` — where state-request primers go.
    fn command_topic(&self) -> String {
        format!("{}/{}/command", self.info.product_type, self.info.serial)
    }

    /// Run forever: connect, subscribe, prime, and pump the EventLoop, emitting
    /// observations onto `tx`. Auto-reconnects on poll errors. Returns only if
    /// the bus closes (router gone / shutdown). `unit_system` is used solely for
    /// the per-observation debug log, matching the other sources.
    pub async fn run(self, unit_system: UnitSystem, tx: mpsc::Sender<Vec<Observation>>) {
        let mut options = MqttOptions::new(
            // Use the serial as the MQTT client id, like libdyson.
            self.info.serial.clone(),
            self.host.clone(),
            self.port,
        );
        options.set_credentials(self.info.serial.clone(), self.info.credential.clone());
        options.set_keep_alive(Duration::from_secs(KEEPALIVE_SECS));
        if self.tls {
            // Future 8883 models: default rustls config. Plaintext 1883 is the
            // path the confirmed broker uses, so this is opt-in.
            options.set_transport(Transport::tls_with_default_config());
        }

        let (client, mut eventloop) = AsyncClient::new(options, 10);
        let status = self.status_topic();
        let faults = self.faults_topic();
        let command = self.command_topic();

        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    info!(host = %self.host, serial = %self.info.serial, "Dyson MQTT connected");
                    // (Re)subscribe and (re)prime on every fresh connection.
                    subscribe(&client, &status, &faults).await;
                    prime_state(&client, &command).await;
                }
                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    if handle_publish(&self.info.serial, &publish.payload, unit_system, &tx)
                        .await
                        .is_err()
                    {
                        // Bus closed (router gone). Stop the task.
                        debug!("event bus closed; Dyson source exiting");
                        return;
                    }
                }
                Ok(_) => { /* pings, suback, outgoing — nothing to map. */ }
                Err(e) => {
                    // Connection dropped or refused. rumqttc retains our
                    // subscriptions and reconnects on the next poll; we back off
                    // briefly to avoid a hot loop while the device is offline.
                    warn!(error = %e, "Dyson MQTT connection error — reconnecting");
                    tokio::time::sleep(RECONNECT_BACKOFF).await;
                }
            }
        }
    }
}

/// Resolve `(serial, product_type, credential)` from config. Explicit overrides
/// take precedence over SSID/password derivation, field by field, so a partial
/// sticker (e.g. credential known but not the SSID) still works.
fn resolve_mqtt_info(cfg: &DysonConfig) -> Result<MqttInfo> {
    // Start from SSID/password derivation when both are present.
    let derived = match (&cfg.ssid, &cfg.wifi_password) {
        (Some(ssid), Some(pw)) => Some(credential::derive(ssid, pw)?),
        _ => None,
    };

    let serial = cfg
        .serial
        .clone()
        .or_else(|| derived.as_ref().map(|d| d.serial.clone()))
        .context("dyson config needs `serial` or a parseable `ssid`")?;
    let product_type = cfg
        .product_type
        .clone()
        .or_else(|| derived.as_ref().map(|d| d.product_type.clone()))
        .context("dyson config needs `product_type` or a parseable `ssid`")?;
    let credential = cfg
        .credential
        .clone()
        .or_else(|| derived.as_ref().map(|d| d.credential.clone()))
        .or_else(|| cfg.wifi_password.as_deref().map(credential::mqtt_credential))
        .context("dyson config needs `credential`, or `wifi_password` to derive it")?;

    Ok(MqttInfo { serial, product_type, credential })
}

/// Subscribe to the status (and best-effort faults) topics. Logged, never fatal.
async fn subscribe(client: &AsyncClient, status: &str, faults: &str) {
    if let Err(e) = client.subscribe(status, QoS::AtMostOnce).await {
        error!(topic = %status, error = %e, "Dyson subscribe failed");
    } else {
        debug!(topic = %status, "subscribed");
    }
    // Faults are optional; a failure here must not affect the status feed.
    if let Err(e) = client.subscribe(faults, QoS::AtMostOnce).await {
        debug!(topic = %faults, error = %e, "Dyson faults subscribe failed (ignored)");
    }
}

/// Publish the two state-request primers to the command topic so the device
/// emits a full `CURRENT-STATE` and environmental snapshot right after connect.
async fn prime_state(client: &AsyncClient, command: &str) {
    for msg in ["REQUEST-CURRENT-STATE", "REQUEST-PRODUCT-ENVIRONMENT-CURRENT-SENSOR-DATA"] {
        let payload = format!(r#"{{"msg":"{msg}","time":"{}"}}"#, mqtt_time());
        if let Err(e) = client
            .publish(command, QoS::AtMostOnce, false, payload.into_bytes())
            .await
        {
            warn!(topic = %command, msg, error = %e, "Dyson prime publish failed");
        } else {
            debug!(topic = %command, msg, "primed device state");
        }
    }
}

/// Parse one incoming publish payload, map it to observations, log, and send the
/// batch onto the bus. `Err(())` means the bus is closed (router gone).
async fn handle_publish(
    serial: &str,
    payload: &[u8],
    unit_system: UnitSystem,
    tx: &mpsc::Sender<Vec<Observation>>,
) -> Result<(), ()> {
    let Some(message) = model::parse(payload) else {
        // Not a state/environmental message we map (ack, unknown type, fault).
        return Ok(());
    };
    let observations = canonical::to_observations(serial, &message.fields);
    if observations.is_empty() {
        return Ok(());
    }
    info!(serial, msg = %message.msg, count = observations.len(), "mapped Dyson observations");
    for obs in &observations {
        debug!(
            entity = %obs.entity,
            class = ?obs.class,
            value = %obs.value.in_system(unit_system),
            "observation",
        );
    }
    tx.send(observations).await.map_err(|_| ())
}

/// libdyson's `mqtt_time()`: UTC, ISO-8601 with a `Z` suffix and second
/// precision — `time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())`.
/// Implemented with `std` only (no chrono dep) by converting the Unix epoch.
fn mqtt_time() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, min, sec) = civil_from_unix(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Convert a Unix timestamp (seconds, UTC) to civil `(y, m, d, h, mi, s)` using
/// Howard Hinnant's `civil_from_days` algorithm. Pure integer math, no deps.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // days since 1970-01-01 -> civil date (Hinnant, "chrono-Compatible Low-Level
    // Date Algorithms"). Shift the era to start on 0000-03-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DysonConfig;

    fn base_cfg() -> DysonConfig {
        DysonConfig {
            host: "192.168.2.133".to_string(),
            port: 1883,
            tls: false,
            ssid: None,
            wifi_password: None,
            serial: None,
            product_type: None,
            credential: None,
        }
    }

    #[test]
    fn resolves_from_ssid_and_password() {
        let cfg = DysonConfig {
            ssid: Some("DYSON-NK6-EU-HHA1111A-438".to_string()),
            wifi_password: Some("hunter2-wifi-pass".to_string()),
            ..base_cfg()
        };
        let info = resolve_mqtt_info(&cfg).unwrap();
        assert_eq!(info.serial, "NK6-EU-HHA1111A");
        assert_eq!(info.product_type, "438");
        assert_eq!(
            info.credential,
            "O04yf88bfnVC4DhoaE+qJwa276NJQe9/sDwGoJ6Y8A1Tnhl3XKlPnXPXeYbOmjHYGm+zRjUVYmzpjaBHKgqYBQ=="
        );
    }

    #[test]
    fn explicit_overrides_win_over_derivation() {
        let cfg = DysonConfig {
            ssid: Some("DYSON-NK6-EU-HHA1111A-438".to_string()),
            wifi_password: Some("hunter2-wifi-pass".to_string()),
            serial: Some("OVERRIDE-SERIAL".to_string()),
            product_type: Some("527".to_string()),
            credential: Some("explicit-cred".to_string()),
            ..base_cfg()
        };
        let info = resolve_mqtt_info(&cfg).unwrap();
        assert_eq!(info.serial, "OVERRIDE-SERIAL");
        assert_eq!(info.product_type, "527");
        assert_eq!(info.credential, "explicit-cred");
    }

    #[test]
    fn topics_are_product_type_and_serial_scoped() {
        let cfg = DysonConfig {
            serial: Some("NK6-EU-HHA1111A".to_string()),
            product_type: Some("438".to_string()),
            credential: Some("cred".to_string()),
            ..base_cfg()
        };
        let src = DysonSource::from_config(&cfg).unwrap();
        assert_eq!(src.status_topic(), "438/NK6-EU-HHA1111A/status/current");
        assert_eq!(src.faults_topic(), "438/NK6-EU-HHA1111A/status/faults");
        assert_eq!(src.command_topic(), "438/NK6-EU-HHA1111A/command");
    }

    #[test]
    fn missing_identity_is_an_error() {
        // No SSID, no overrides -> can't resolve.
        assert!(resolve_mqtt_info(&base_cfg()).is_err());
    }

    #[test]
    fn mqtt_time_is_iso8601_utc_z() {
        // A fixed epoch second -> known UTC civil time.
        // 1718064000 = 2024-06-11T00:00:00Z.
        let (y, m, d, h, mi, s) = civil_from_unix(1_718_064_000);
        assert_eq!((y, m, d, h, mi, s), (2024, 6, 11, 0, 0, 0));

        // The live formatter shape: YYYY-MM-DDTHH:MM:SSZ (20 chars).
        let now = mqtt_time();
        assert_eq!(now.len(), 20);
        assert!(now.ends_with('Z'));
        assert_eq!(now.as_bytes()[4], b'-');
        assert_eq!(now.as_bytes()[10], b'T');
    }
}
