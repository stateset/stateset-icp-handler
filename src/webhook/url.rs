//! Destination URL validation and SSRF guards.

use std::net::IpAddr;

pub fn validate_destination_url(url: &str, allow_insecure: bool) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| "url must be a valid absolute http(s) URL".to_string())?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("url must use http:// or https://".into()),
    }
    if parsed.host_str().is_none() {
        return Err("url must include a host".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("url must not include credentials".into());
    }
    if !allow_insecure && parsed.scheme() != "https" {
        return Err("url must use https:// when insecure URLs are disabled".into());
    }
    if !allow_insecure {
        let host = parsed.host_str().expect("host checked");
        if is_forbidden_host(host) {
            return Err("url host must not resolve to localhost or a private network".into());
        }
    }
    Ok(())
}

/// Resolve `url`'s host and refuse it if the literal address — or *any*
/// address DNS returns for it — points at localhost or a private/internal
/// network. This is the SSRF gate shared by the webhook worker and the
/// `did:web` resolver, so the two outbound-fetch paths can't drift apart
/// (the resolver previously had no guard at all, making it a blind-SSRF
/// primitive reachable by any authenticated mandate `iss`).
///
/// Note the residual DNS-rebinding TOCTOU: the caller resolves here, then
/// the HTTP client resolves again independently. Closing that fully needs
/// connection-time IP pinning; this check still blocks the overwhelming
/// majority of SSRF attempts (literal internal IPs, metadata endpoints,
/// internal hostnames).
pub(crate) async fn ensure_public_url(url: &str) -> Result<(), String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| "url must be a valid absolute URL".to_string())?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "url must include a host".to_string())?;
    if is_forbidden_host(host) {
        return Err("url host must not resolve to localhost or a private network".into());
    }
    // Literal IPs are fully vetted by is_forbidden_host above; only names
    // need a DNS round-trip.
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "url must include a resolvable port".to_string())?;
    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("dns resolution failed: {e}"))?;
    let mut saw_addr = false;
    for addr in addrs.by_ref() {
        saw_addr = true;
        if is_forbidden_ip(addr.ip()) {
            return Err("url host must not resolve to localhost or a private network".into());
        }
    }
    if !saw_addr {
        return Err("url host did not resolve to any address".into());
    }
    Ok(())
}

pub(crate) fn is_forbidden_host(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "localhost" | "ip6-localhost" | "ip6-loopback"
    ) || normalized.ends_with(".localhost")
        || normalized.ends_with(".local")
    {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(is_forbidden_ip)
}

pub(crate) fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}
