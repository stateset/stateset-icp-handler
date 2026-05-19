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
