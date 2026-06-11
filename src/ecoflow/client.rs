use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::ecoflow::model::{DeviceListEntry, Envelope, QuotaAll};

type HmacSha256 = Hmac<Sha256>;

/// Default EcoFlow IoT Open API host (US/global). All signed endpoints live
/// under `/iot-open/sign`.
const DEFAULT_BASE_URL: &str = "https://api-e.ecoflow.com";

/// Thin client over the EcoFlow IoT Open API with HMAC-SHA256 request signing.
///
/// ## Signing algorithm (verified against `tolwi/hassio-ecoflow-cloud`'s
/// `public_api.py` — `__gen_sign` / `__sort_and_concat_params` /
/// `__encrypt_hmac_sha256` — and EcoFlow's official developer docs)
///
/// 1. Flatten request params into `key=value` pairs. Nested objects use
///    `parent.child`; arrays use `key[index]` (and array-of-objects
///    `key[index].field`). EcoFlow's own example string is:
///    `deviceInfo.id=1&deviceList[0].id=1&deviceList[1].id=2&ids[0]=1&ids[1]=2&ids[2]=3&name=demo1`
/// 2. Sort the flattened keys ASCII-ascending and join as `k=v` with `&`.
/// 3. Append the credential triple, in this fixed order, after the params:
///    `accessKey=<accessKey>&nonce=<nonce>&timestamp=<timestamp>`
///    (when there are no params, the string-to-sign is just that triple).
/// 4. `sign = lowercase_hex(HMAC_SHA256(string_to_sign, key = secretKey))`.
/// 5. Send `accessKey`, `nonce`, `timestamp`, `sign` as HTTP headers.
///
/// For the two GET endpoints we implement the params are flat (`device/list`
/// takes none; `device/quota/all` takes a single string `sn`), but the signer
/// implements the full flattening so it stays correct for future POST bodies.
pub struct EcoflowClient {
    http: reqwest::Client,
    base_url: String,
    access_key: String,
    secret_key: String,
}

impl EcoflowClient {
    pub fn new(access_key: impl Into<String>, secret_key: impl Into<String>) -> Result<Self> {
        Self::with_base_url(access_key, secret_key, DEFAULT_BASE_URL)
    }

    pub fn with_base_url(
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base_url: base_url.into(),
            access_key: access_key.into(),
            secret_key: secret_key.into(),
        })
    }

    /// `GET /iot-open/sign/device/list` — the serial numbers bound to this
    /// EcoFlow developer account. Takes no query params.
    pub async fn device_list(&self) -> Result<Vec<DeviceListEntry>> {
        let env: Envelope<Vec<DeviceListEntry>> =
            self.get_signed("/iot-open/sign/device/list", &[]).await?;
        if !env.is_ok() {
            bail!(
                "EcoFlow device/list returned code {} ({})",
                env.code,
                env.message.as_deref().unwrap_or("no message"),
            );
        }
        Ok(env.data.unwrap_or_default())
    }

    /// `GET /iot-open/sign/device/quota/all?sn=<sn>` — the device's full live
    /// state as a flat `"property.path" -> scalar` map.
    pub async fn quota_all(&self, sn: &str) -> Result<QuotaAll> {
        let env: Envelope<QuotaAll> = self
            .get_signed("/iot-open/sign/device/quota/all", &[("sn", sn)])
            .await?;
        if !env.is_ok() {
            bail!(
                "EcoFlow device/quota/all returned code {} ({}) for sn={sn}",
                env.code,
                env.message.as_deref().unwrap_or("no message"),
            );
        }
        Ok(env.data.unwrap_or_default())
    }

    /// Sign and send a GET to `path` with the given (already-flat) query params,
    /// decoding the JSON envelope into `T`.
    async fn get_signed<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<Envelope<T>> {
        let nonce = generate_nonce();
        let timestamp = current_timestamp_ms();
        let query = sort_and_concat_params(params);
        let sign = self.sign(&query, &nonce, &timestamp);

        // The query string sent on the wire must be byte-identical to the one
        // that was signed, so we attach `query` ourselves rather than via
        // reqwest's `.query()` (which would re-encode/re-order).
        let url = if query.is_empty() {
            format!("{}{path}", self.base_url)
        } else {
            format!("{}{path}?{query}", self.base_url)
        };

        self.http
            .get(url)
            .header("accessKey", &self.access_key)
            .header("nonce", &nonce)
            .header("timestamp", &timestamp)
            .header("sign", &sign)
            .send()
            .await
            .context("sending request to EcoFlow")?
            .error_for_status()
            .context("EcoFlow returned an error status")?
            .json()
            .await
            .context("decoding EcoFlow response")
    }

    /// Build the `sign` header value for a request: the lowercase-hex
    /// HMAC-SHA256 of the canonical string-to-sign, keyed by `secretKey`.
    fn sign(&self, query: &str, nonce: &str, timestamp: &str) -> String {
        let to_sign = string_to_sign(query, &self.access_key, nonce, timestamp);
        hmac_sha256_hex(self.secret_key.as_bytes(), to_sign.as_bytes())
    }
}

/// Sort params by key (ASCII-ascending) and concatenate as `k=v` joined by `&`.
/// Inputs here are already flat; see [`flatten_params`] for nested values.
fn sort_and_concat_params(params: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(&str, &str)> = params.to_vec();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Build EcoFlow's canonical string-to-sign: the sorted `query` (may be empty)
/// followed by the credential triple in fixed order. Matches `__gen_sign`:
/// `target = "accessKey=..&nonce=..&timestamp=.."; if query: target = query + "&" + target`.
fn string_to_sign(query: &str, access_key: &str, nonce: &str, timestamp: &str) -> String {
    let creds = format!("accessKey={access_key}&nonce={nonce}&timestamp={timestamp}");
    if query.is_empty() {
        creds
    } else {
        format!("{query}&{creds}")
    }
}

/// HMAC-SHA256(message) keyed by `key`, lowercase-hex encoded.
fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(message);
    hex::encode(mac.finalize().into_bytes())
}

/// EcoFlow nonce: a short random numeric string. `tolwi` uses
/// `random.randint(10000, 1000000)`; we mirror that range. Only used as a
/// salt — the server does not require a specific format.
fn generate_nonce() -> String {
    // Derive a value in [10000, 1000000] from the wall clock without pulling in
    // a `rand` dependency. Collisions across requests are harmless (the
    // timestamp also varies), and tests never go through this path.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = 10_000 + (now % 990_001);
    n.to_string()
}

/// Current Unix time in milliseconds, as the API expects in the `timestamp`
/// header / string-to-sign.
fn current_timestamp_ms() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        .to_string()
}

/// Flatten a JSON value into EcoFlow's signing pairs. Objects nest with
/// `parent.child`, arrays index with `key[i]`, scalars terminate. Exposed for
/// completeness/testing and future signed POST bodies; the GET endpoints here
/// only ever pass flat string params.
#[allow(dead_code)]
pub fn flatten_params(value: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    flatten_into(String::new(), value, &mut out);
    out
}

#[allow(dead_code)]
fn flatten_into(prefix: String, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten_into(key, v, out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                flatten_into(format!("{prefix}[{i}]"), v, out);
            }
        }
        Value::String(s) => out.push((prefix, s.clone())),
        Value::Null => out.push((prefix, String::new())),
        other => out.push((prefix, other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Fixed test vector -------------------------------------------------
    //
    // accessKey / secretKey / nonce / timestamp are FIXED here so the expected
    // signature is deterministic and requires no network or real credentials.
    //
    // Vector chosen from EcoFlow's documented example query plus arbitrary,
    // committed credentials:
    const ACCESS_KEY: &str = "Fp4SvIprYSDPXtYJidEtUAd1o";
    const SECRET_KEY: &str = "WIbFEKre0s6sLnh4ei7SPUeYnptHG6V";
    const NONCE: &str = "345164";
    const TIMESTAMP: &str = "1671171709428";

    #[test]
    fn string_to_sign_appends_creds_after_sorted_params() {
        // EcoFlow's own documented example params, pre-sorted ASCII-ascending.
        let query = "params.cmdSet=11&params.eps=0&params.id=24&sn=123456789";
        let s = string_to_sign(query, ACCESS_KEY, NONCE, TIMESTAMP);
        assert_eq!(
            s,
            "params.cmdSet=11&params.eps=0&params.id=24&sn=123456789\
             &accessKey=Fp4SvIprYSDPXtYJidEtUAd1o&nonce=345164&timestamp=1671171709428",
        );
    }

    #[test]
    fn string_to_sign_with_no_params_is_just_creds() {
        // The `device/list` case: no query params.
        let s = string_to_sign("", ACCESS_KEY, NONCE, TIMESTAMP);
        assert_eq!(
            s,
            "accessKey=Fp4SvIprYSDPXtYJidEtUAd1o&nonce=345164&timestamp=1671171709428",
        );
    }

    #[test]
    fn hmac_signature_matches_fixed_expected_hex() {
        // String-to-sign (no params — the device/list request):
        //   accessKey=Fp4SvIprYSDPXtYJidEtUAd1o&nonce=345164&timestamp=1671171709428
        //
        // sign = lowercase_hex(HMAC_SHA256(string_to_sign, key = SECRET_KEY)).
        // The expected value below is computed from this exact algorithm; if the
        // signer regresses (wrong order, wrong key, uppercase hex), it changes.
        let to_sign = string_to_sign("", ACCESS_KEY, NONCE, TIMESTAMP);
        let sign = hmac_sha256_hex(SECRET_KEY.as_bytes(), to_sign.as_bytes());
        assert_eq!(
            sign,
            "4409ae9efeaf0be9b8b48f02ebef8e804387e745e513588c136955606ca496f5",
        );
        // 64 lowercase hex chars, by construction.
        assert_eq!(sign.len(), 64);
        assert!(sign.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn hmac_signature_with_params_matches_fixed_expected_hex() {
        // The `device/quota/all?sn=123456789` request. String-to-sign:
        //   sn=123456789&accessKey=Fp4SvIprYSDPXtYJidEtUAd1o&nonce=345164&timestamp=1671171709428
        let to_sign = string_to_sign("sn=123456789", ACCESS_KEY, NONCE, TIMESTAMP);
        assert_eq!(
            to_sign,
            "sn=123456789&accessKey=Fp4SvIprYSDPXtYJidEtUAd1o&nonce=345164&timestamp=1671171709428",
        );
        let sign = hmac_sha256_hex(SECRET_KEY.as_bytes(), to_sign.as_bytes());
        assert_eq!(
            sign,
            "2296b02874dc12a165e0691e8f0c76c4424c582b335b48cd9c95518bebefbef5",
        );
    }

    #[test]
    fn sort_and_concat_orders_keys_ascii() {
        let params = [("sn", "123"), ("cmdSet", "11"), ("id", "24")];
        assert_eq!(sort_and_concat_params(&params), "cmdSet=11&id=24&sn=123");
    }

    #[test]
    fn sort_and_concat_empty_is_empty() {
        assert_eq!(sort_and_concat_params(&[]), "");
    }

    #[test]
    fn flatten_handles_objects_arrays_and_scalars() {
        use serde_json::json;
        // EcoFlow's documented nesting example, value-by-value.
        let v = json!({
            "deviceInfo": {"id": 1},
            "deviceList": [{"id": 1}, {"id": 2}],
            "ids": [1, 2, 3],
            "name": "demo1"
        });
        let mut pairs = flatten_params(&v);
        pairs.sort();
        let flat: Vec<String> = pairs.iter().map(|(k, val)| format!("{k}={val}")).collect();
        assert_eq!(
            flat,
            vec![
                "deviceInfo.id=1",
                "deviceList[0].id=1",
                "deviceList[1].id=2",
                "ids[0]=1",
                "ids[1]=2",
                "ids[2]=3",
                "name=demo1",
            ],
        );
    }
}
