//! HMAC-SHA256 webhook signing (Stripe-style `t=<unix>,v1=<hex>`).

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Compute the value of the `ICP-Signature` header for a delivery.
/// Format mirrors Stripe's: `t=<unix>,v1=<hex>`.
pub fn sign(secret: &str, timestamp_unix: i64, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC-SHA256 key length is unbounded");
    let signing_input = format!("{timestamp_unix}.");
    mac.update(signing_input.as_bytes());
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    format!("t={timestamp_unix},v1={}", hex::encode(tag))
}

/// Receiver-side helper: returns true iff the supplied header value
/// verifies against `secret` for the given body.
pub fn verify(secret: &str, header_value: &str, body: &[u8]) -> bool {
    // Parse `t=<unix>,v1=<hex>`; tolerate extra fields.
    let mut t = None;
    let mut v1 = None;
    for part in header_value.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("t=") {
            t = rest.parse::<i64>().ok();
        } else if let Some(rest) = part.strip_prefix("v1=") {
            v1 = Some(rest.to_string());
        }
    }
    let (Some(ts), Some(supplied)) = (t, v1) else {
        return false;
    };
    let expected = sign(secret, ts, body);
    // Compare the v1 portion only.
    let expected_v1 = expected
        .split(',')
        .find_map(|p| p.strip_prefix("v1=").map(str::to_string))
        .unwrap_or_default();
    constant_time_eq(supplied.as_bytes(), expected_v1.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
