//! The opt-in `url` source mode, and its address guard.
//!
//! Off by default. When an operator turns it on, a caller-supplied URL
//! becomes an outbound request from inside the gateway, which is the shape of
//! a request-forgery primitive. Three things keep it bounded:
//!
//! - **every hop is re-resolved and re-checked**, so a redirect to
//!   `169.254.169.254` is refused even though the first hop was public;
//! - **private, loopback, link-local and unspecified addresses are refused**
//!   unless the profile explicitly opts in, which is what closes the
//!   cloud-metadata path;
//! - an optional **host allowlist** narrows it further.
//!
//! Note what is *not* here: nothing fetches a URL found **inside** a
//! document. An `<img src>` in converted HTML is rendered as a link and never
//! requested. That is the difference between a caller asking for a URL and a
//! document asking on their behalf.

use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use mcpg_markdown_convert::StreamInfo;
use mcpg_plugin_protocol::BackendError;

use crate::config::ProfileConfig;
use crate::source::{Acquired, invalid};

/// Fetch `url`, following redirects one hop at a time so each target can be
/// checked before it is contacted.
pub fn get(url: &str, cfg: &ProfileConfig, info: StreamInfo) -> Result<Acquired, BackendError> {
    let opts = &cfg.url;
    let mut current = url.to_owned();

    for _hop in 0..=opts.max_redirects {
        guard(&current, cfg)?;

        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(opts.timeout_ms)))
            // Redirects are followed here, not by the agent, so the guard
            // runs against every hop rather than only the first.
            .max_redirects(0)
            .build()
            .new_agent();

        let mut response = match agent.get(&current).call() {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(code)) if (300..400).contains(&code) => {
                return Err(invalid(format!(
                    "{current} redirected with status {code} but sent no Location header"
                )));
            }
            Err(e) => {
                return Err(BackendError::Transport {
                    message: format!("fetching {current}: {e}"),
                });
            }
        };

        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    invalid(format!(
                        "{current} returned {status} with no Location header"
                    ))
                })?;
            current = resolve_redirect(&current, location)?;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(BackendError::Transport {
                message: format!("{current} returned HTTP {status}"),
            });
        }

        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let limit = cfg.convert.limits.max_input_bytes;
        let mut bytes = Vec::new();
        // One byte past the ceiling so an oversized body is refused rather
        // than silently truncated into a half-document.
        response
            .body_mut()
            .as_reader()
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| BackendError::Transport {
                message: format!("reading {current}: {e}"),
            })?;
        if bytes.len() as u64 > limit {
            return Err(invalid(format!(
                "{current} is larger than max_input_bytes ({limit})"
            )));
        }

        let mut info = info;
        if info.mimetype.is_none()
            && let Some(m) = mime
        {
            info = info.with_mimetype(m);
        }
        if info.filename.is_none()
            && let Some(name) = filename_from_url(&current)
        {
            info = info.with_filename(name);
        }
        return Ok(Acquired {
            bytes,
            info: info.with_url(current),
            mode: "url",
        });
    }

    Err(invalid(format!(
        "{url} exceeded max_redirects ({})",
        opts.max_redirects
    )))
}

/// Check one URL against the profile's policy.
fn guard(url: &str, cfg: &ProfileConfig) -> Result<(), BackendError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| invalid(format!("malformed url {url:?}")))?;
    if scheme != "http" && scheme != "https" {
        return Err(invalid(format!("refusing scheme {scheme:?}")));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = split_host_port(authority, scheme);
    if host.is_empty() {
        return Err(invalid(format!("malformed url {url:?}")));
    }

    if !cfg.url.allow_hosts.is_empty() {
        let allowed = cfg
            .url
            .allow_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&host));
        if !allowed {
            return Err(invalid(format!(
                "{host} is not in the profile's url.allow_hosts"
            )));
        }
    }

    if cfg.url.allow_private_addresses {
        return Ok(());
    }

    // Resolve here and check every answer. A name that resolves to one public
    // and one private address must be refused, not sampled.
    let addrs: Vec<IpAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| invalid(format!("{host} did not resolve: {e}")))?
        .map(|s| s.ip())
        .collect();
    if addrs.is_empty() {
        return Err(invalid(format!("{host} resolved to no addresses")));
    }
    for ip in addrs {
        if !is_public(ip) {
            return Err(invalid(format!(
                "{host} resolves to the non-public address {ip}; \
                 set url.allow_private_addresses to permit it"
            )));
        }
    }
    Ok(())
}

fn split_host_port(authority: &str, scheme: &str) -> (String, u16) {
    let default = if scheme == "https" { 443 } else { 80 };
    // IPv6 literals are bracketed, and their colons are not port separators.
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').unwrap_or((rest, ""));
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default);
        return (host.to_owned(), port);
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().unwrap_or(default)),
        None => (authority.to_owned(), default),
    }
}

/// Everything not routable on the public internet is refused. Written out
/// rather than delegated so the list is reviewable in one place.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                // 100.64.0.0/10, carrier-grade NAT — reaches other tenants.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                // 192.0.0.0/24, IETF protocol assignments.
                || (v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0)
                // 198.18.0.0/15, benchmarking.
                || (v4.octets()[0] == 198 && (18..20).contains(&v4.octets()[1]))
                // 240.0.0.0/4, reserved.
                || v4.octets()[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            // An IPv4-mapped address is an IPv4 address wearing a hat.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let seg = v6.segments()[0];
            // fc00::/7 unique-local, fe80::/10 link-local.
            !((seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80)
        }
    }
}

/// Resolve a `Location` header against the URL that produced it.
fn resolve_redirect(base: &str, location: &str) -> Result<String, BackendError> {
    let loc = location.trim();
    if loc.contains("://") {
        return Ok(loc.to_owned());
    }
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| invalid(format!("malformed url {base:?}")))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if let Some(abs) = loc.strip_prefix('/') {
        return Ok(format!("{scheme}://{authority}/{abs}"));
    }
    let dir = rest
        .rsplit_once('/')
        .map_or(String::new(), |(d, _)| format!("{d}/"));
    Ok(format!("{scheme}://{dir}{loc}"))
}

fn filename_from_url(url: &str) -> Option<String> {
    let path = url.split_once("://")?.1;
    let path = path.split(['?', '#']).next()?;
    let last = path.rsplit('/').next()?;
    if last.is_empty() || !last.contains('.') {
        return None;
    }
    Some(last.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UrlOptions;

    fn cfg(url: UrlOptions) -> ProfileConfig {
        ProfileConfig {
            url,
            ..ProfileConfig::default()
        }
    }

    #[test]
    fn loopback_and_metadata_addresses_are_refused() {
        let c = cfg(UrlOptions::default());
        for url in [
            "http://127.0.0.1/x",
            "http://localhost/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/x",
        ] {
            let e = guard(url, &c);
            assert!(e.is_err(), "{url} was allowed");
        }
    }

    #[test]
    fn private_ranges_are_refused() {
        let c = cfg(UrlOptions::default());
        for url in [
            "http://10.0.0.1/x",
            "http://192.168.1.1/x",
            "http://172.16.0.1/x",
            "http://100.64.0.1/x",
        ] {
            assert!(guard(url, &c).is_err(), "{url} was allowed");
        }
    }

    #[test]
    fn an_ipv4_mapped_ipv6_loopback_is_still_loopback() {
        assert!(!is_public("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_public("::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn unique_local_and_link_local_ipv6_are_refused() {
        assert!(!is_public("fd00::1".parse().unwrap()));
        assert!(!is_public("fe80::1".parse().unwrap()));
        assert!(is_public("2606:4700::1111".parse().unwrap()));
    }

    #[test]
    fn opting_in_permits_private_addresses() {
        let c = cfg(UrlOptions {
            allow_private_addresses: true,
            ..UrlOptions::default()
        });
        assert!(guard("http://127.0.0.1/x", &c).is_ok());
    }

    #[test]
    fn non_http_schemes_are_refused() {
        let c = cfg(UrlOptions {
            allow_private_addresses: true,
            ..UrlOptions::default()
        });
        for url in ["file:///etc/passwd", "gopher://x/", "ftp://x/"] {
            assert!(guard(url, &c).is_err(), "{url} was allowed");
        }
    }

    #[test]
    fn the_host_allowlist_is_applied_before_resolution() {
        let c = cfg(UrlOptions {
            allow_hosts: vec!["docs.example.test".to_owned()],
            allow_private_addresses: true,
            ..UrlOptions::default()
        });
        assert!(guard("https://docs.example.test/a.pdf", &c).is_ok());
        let e = guard("https://evil.example.test/a.pdf", &c).unwrap_err();
        assert!(format!("{e:?}").contains("allow_hosts"), "{e:?}");
    }

    #[test]
    fn credentials_in_the_authority_do_not_hide_the_host() {
        // `http://allowed.test@evil.test/` points at evil.test.
        let c = cfg(UrlOptions {
            allow_hosts: vec!["allowed.test".to_owned()],
            allow_private_addresses: true,
            ..UrlOptions::default()
        });
        assert!(guard("http://allowed.test@evil.test/x", &c).is_err());
    }

    #[test]
    fn host_and_port_split_handles_ipv6_literals() {
        assert_eq!(split_host_port("[::1]:8080", "http"), ("::1".into(), 8080));
        assert_eq!(split_host_port("[::1]", "https"), ("::1".into(), 443));
        assert_eq!(split_host_port("h:99", "http"), ("h".into(), 99));
        assert_eq!(split_host_port("h", "http"), ("h".into(), 80));
    }

    #[test]
    fn redirects_resolve_relative_and_absolute_locations() {
        let base = "https://example.test/docs/a.html";
        assert_eq!(
            resolve_redirect(base, "https://other.test/b").unwrap(),
            "https://other.test/b"
        );
        assert_eq!(
            resolve_redirect(base, "/root/b").unwrap(),
            "https://example.test/root/b"
        );
        assert_eq!(
            resolve_redirect(base, "b.html").unwrap(),
            "https://example.test/docs/b.html"
        );
    }

    #[test]
    fn a_filename_is_derived_from_the_path_when_it_has_one() {
        assert_eq!(
            filename_from_url("https://x.test/a/report.pdf?v=2"),
            Some("report.pdf".to_owned())
        );
        assert_eq!(filename_from_url("https://x.test/a/"), None);
        assert_eq!(filename_from_url("https://x.test/noext"), None);
    }

    #[test]
    fn public_addresses_pass() {
        assert!(is_public("8.8.8.8".parse().unwrap()));
        assert!(is_public("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn reserved_v4_ranges_are_refused() {
        assert!(!is_public("240.0.0.1".parse().unwrap()));
        assert!(!is_public("198.18.0.1".parse().unwrap()));
        assert!(!is_public("192.0.0.1".parse().unwrap()));
        assert!(!is_public("0.0.0.0".parse().unwrap()));
    }
}
