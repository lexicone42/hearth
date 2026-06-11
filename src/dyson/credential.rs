//! Fully-local Dyson MQTT credential derivation — NO cloud round-trip.
//!
//! Mirrors libdyson's `get_mqtt_info_from_wifi_info(ssid, wifi_password)`
//! (verified against `shenxn/libdyson` `libdyson/utils.py`, and the maintained
//! fork `libdyson-wg/libdyson-neon`):
//!
//! ```python
//! # libdyson/utils.py — get_mqtt_info_from_wifi_info
//! _DEVICE_TYPE_MAP = {"455A": "455"}
//!
//! def get_mqtt_info_from_wifi_info(ssid: str, wifi_password: str):
//!     # 360 Eye SSID: "(360EYE-)?<serial>"
//!     # Other devices: "DYSON-<serial>-<product_type>"
//!     for prefix, regex in (...):
//!         result = re.match(regex, ssid)
//!         if result:
//!             ...
//!     serial = result.group("serial")
//!     device_type = _DEVICE_TYPE_MAP.get(device_type, device_type)
//!     hash_ = hashlib.sha512()
//!     hash_.update(wifi_password.encode("utf-8"))
//!     credential = base64.b64encode(hash_.digest()).decode("utf-8")
//!     return serial, device_type, credential
//! ```
//!
//! The credential is a single-pass, unsalted `base64(SHA-512(wifi_password))`.
//! The serial and numeric product type are parsed out of the setup SSID, whose
//! "other devices" form (the only one we target) is:
//!   `DYSON-<serial>-<product_type>` where
//!   `serial = [0-9A-Z]{3}-[A-Z]{2}-[0-9A-Z]{8}` and
//!   `product_type = [0-9]{3}[A-Z]?` (e.g. `438`, `455A`, `527E`).

use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha512};

/// The MQTT identity for a Dyson device, all derived locally from sticker values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MqttInfo {
    /// Device serial number (also the MQTT username), e.g. `NK6-EU-HHA1111A`.
    pub serial: String,
    /// Numeric product type (the MQTT topic root), e.g. `438`. libdyson maps a
    /// few Wi-Fi-form codes to their MQTT form (`455A` -> `455`).
    pub product_type: String,
    /// MQTT password = `base64(SHA-512(wifi_password))`, single pass, no salt.
    pub credential: String,
}

/// libdyson's `_DEVICE_TYPE_MAP`: a small set of Wi-Fi-sticker product codes
/// that differ from the MQTT topic root. Verified against
/// `libdyson/utils.py::_DEVICE_TYPE_MAP` — currently just `455A` -> `455`.
fn map_product_type(raw: &str) -> String {
    match raw {
        "455A" => "455".to_string(),
        other => other.to_string(),
    }
}

/// Derive `(serial, product_type, credential)` from the setup SSID + Wi-Fi
/// password exactly as libdyson does. Only the non-360-Eye `DYSON-...` SSID form
/// is supported (the air purifiers/fans this source targets).
pub fn derive(ssid: &str, wifi_password: &str) -> Result<MqttInfo> {
    let (serial, raw_type) = parse_ssid(ssid)?;
    Ok(MqttInfo {
        serial,
        product_type: map_product_type(&raw_type),
        credential: mqtt_credential(wifi_password),
    })
}

/// MQTT password = `base64(SHA-512(wifi_password_bytes))`. Single SHA-512 pass,
/// no salt, standard base64 (with `+`/`/` and `=` padding), matching
/// `base64.b64encode(hashlib.sha512(pw).digest())` in libdyson.
pub fn mqtt_credential(wifi_password: &str) -> String {
    let digest = Sha512::digest(wifi_password.as_bytes());
    BASE64.encode(digest)
}

/// Parse the `DYSON-<serial>-<product_type>` setup SSID into `(serial, raw
/// product type)`, following libdyson's exact regex/split:
///   `^DYSON-([0-9A-Z]{3}-[A-Z]{2}-[0-9A-Z]{8})-([0-9]{3}[A-Z]?)$`
///
/// The serial itself contains hyphens (`XXX-YY-ZZZZZZZZ`), so we can't just
/// split on `-`; we strip the `DYSON-` prefix, then split off the final `-`
/// segment as the product type and validate both halves against libdyson's
/// character classes. (Implemented without a regex crate to keep deps minimal;
/// the validation reproduces the regex's shape exactly.)
fn parse_ssid(ssid: &str) -> Result<(String, String)> {
    let Some(rest) = ssid.strip_prefix("DYSON-") else {
        bail!("Dyson SSID must start with 'DYSON-' (got {ssid:?})");
    };
    // Product type is the segment after the LAST '-'; everything before is the
    // serial (which itself contains two '-').
    let Some((serial, raw_type)) = rest.rsplit_once('-') else {
        bail!("Dyson SSID {ssid:?} is missing the product-type suffix");
    };
    if !is_valid_serial(serial) {
        bail!(
            "Dyson SSID serial {serial:?} doesn't match [0-9A-Z]{{3}}-[A-Z]{{2}}-[0-9A-Z]{{8}}"
        );
    }
    if !is_valid_product_type(raw_type) {
        bail!("Dyson SSID product type {raw_type:?} doesn't match [0-9]{{3}}[A-Z]?");
    }
    Ok((serial.to_string(), raw_type.to_string()))
}

/// `[0-9A-Z]{3}-[A-Z]{2}-[0-9A-Z]{8}` — three alnum, two upper-alpha, eight alnum.
fn is_valid_serial(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    let [a, b, c] = [parts[0], parts[1], parts[2]];
    a.len() == 3
        && a.bytes().all(is_upper_alnum)
        && b.len() == 2
        && b.bytes().all(|x| x.is_ascii_uppercase())
        && c.len() == 8
        && c.bytes().all(is_upper_alnum)
}

/// `[0-9]{3}[A-Z]?` — three digits, then an optional single uppercase letter.
fn is_valid_product_type(s: &str) -> bool {
    let bytes = s.as_bytes();
    match bytes.len() {
        3 => bytes.iter().all(|b| b.is_ascii_digit()),
        4 => bytes[..3].iter().all(|b| b.is_ascii_digit()) && bytes[3].is_ascii_uppercase(),
        _ => false,
    }
}

fn is_upper_alnum(b: u8) -> bool {
    b.is_ascii_digit() || b.is_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_is_unsalted_base64_sha512() {
        // Independently-computed expected value:
        //   python3 -c "import hashlib,base64; \
        //     print(base64.b64encode(hashlib.sha512(b'hunter2-wifi-pass').digest()).decode())"
        // -> the constant below. This is a single SHA-512 pass, no salt, std base64.
        let cred = mqtt_credential("hunter2-wifi-pass");
        assert_eq!(
            cred,
            "O04yf88bfnVC4DhoaE+qJwa276NJQe9/sDwGoJ6Y8A1Tnhl3XKlPnXPXeYbOmjHYGm+zRjUVYmzpjaBHKgqYBQ=="
        );
        // SHA-512 -> 64 bytes -> 88-char base64 (with two '=' pad chars).
        assert_eq!(cred.len(), 88);
    }

    #[test]
    fn derives_serial_type_and_credential_from_ssid() {
        // Synthetic but well-formed setup SSID + password.
        let info = derive("DYSON-NK6-EU-HHA1111A-438", "hunter2-wifi-pass").unwrap();
        assert_eq!(info.serial, "NK6-EU-HHA1111A");
        assert_eq!(info.product_type, "438");
        assert_eq!(
            info.credential,
            "O04yf88bfnVC4DhoaE+qJwa276NJQe9/sDwGoJ6Y8A1Tnhl3XKlPnXPXeYbOmjHYGm+zRjUVYmzpjaBHKgqYBQ=="
        );
    }

    #[test]
    fn maps_455a_product_type_to_455() {
        // libdyson's only _DEVICE_TYPE_MAP entry: the Wi-Fi code 455A -> MQTT 455.
        let info = derive("DYSON-ABC-DE-FGHJ1234-455A", "pw").unwrap();
        assert_eq!(info.product_type, "455");
        assert_eq!(info.serial, "ABC-DE-FGHJ1234");
    }

    #[test]
    fn rejects_malformed_ssids() {
        assert!(derive("NK6-EU-HHA1111A-438", "pw").is_err()); // missing DYSON- prefix
        assert!(derive("DYSON-NK6-EU-HHA1111A", "pw").is_err()); // missing product type
        assert!(derive("DYSON-NK6-E-HHA1111A-438", "pw").is_err()); // bad serial (1-char region)
        assert!(derive("DYSON-NK6-EU-HHA1111A-43", "pw").is_err()); // 2-digit product type
    }
}
