//! Host-side `net.fetch` broker.
//!
//! A plugin never gets a raw HTTP client as a *capability*. It asks the host to
//! fetch a URL, and the host applies the grant's scope before any socket is
//! opened: host allowlist, method, byte cap, timeout, redirect policy, SSRF
//! checks and an audit row. Phase 2.1 keeps the control channel on loopback HTTP
//! (phase 3 replaces it), but the fetch itself is already host-controlled.

use std::collections::BTreeMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::capability::NetFetchScope;

/// One fetch a plugin asks the host to perform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerRequest {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    /// Only a small allowlist is forwarded; see [`ALLOWED_HEADERS`].
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Handle of a host-held secret to inject. The value never reaches the
    /// plugin; the host resolves it only after checking the `secret.use` grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

fn default_method() -> String {
    "GET".to_string()
}

impl BrokerRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: "GET".into(),
            headers: BTreeMap::new(),
            secret: None,
        }
    }

    pub fn with_secret(mut self, handle: impl Into<String>) -> Self {
        self.secret = Some(handle.into());
        self
    }
}

/// The host's answer. The API encodes `body` as base64 for the plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Why a broker call failed. The host maps this to an API error and an audit
/// row; the plugin only sees a stable code/message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerError {
    Invalid(String),
    Denied(String),
    Redirect(String),
    TooLarge(u64),
    Timeout,
    Upstream(String),
}

impl BrokerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::Denied(_) => "denied",
            Self::Redirect(_) => "redirect_not_allowed",
            Self::TooLarge(_) => "response_too_large",
            Self::Timeout => "upstream_timeout",
            Self::Upstream(_) => "upstream_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Invalid(message) | Self::Denied(message) | Self::Redirect(message) => {
                message.clone()
            }
            Self::TooLarge(limit) => format!("response exceeds the {limit} byte scope limit"),
            Self::Timeout => "upstream request timed out".to_string(),
            Self::Upstream(message) => message.clone(),
        }
    }
}

/// Request headers a plugin may set. Cookie/Authorization belong to
/// `secret.use`, which injects them host-side; Host/Content-Length are owned by
/// the HTTP client.
pub const ALLOWED_HEADERS: &[&str] = &["accept", "accept-language", "user-agent", "referer"];

/// Fetch one URL under the approved scope.
///
/// Pure with respect to the host: no database, no audit. The caller
/// (`PluginHost`) owns grants, rate limiting and attribution.
pub fn fetch(
    scope: &NetFetchScope,
    request: &BrokerRequest,
) -> Result<BrokerResponse, BrokerError> {
    fetch_inner(scope, request, false)
}

/// Test-only entry point that keeps the HTTP/redirect logic exercisable against
/// a loopback fixture without weakening the production SSRF check.
#[cfg(test)]
fn fetch_loopback_for_test(
    scope: &NetFetchScope,
    request: &BrokerRequest,
) -> Result<BrokerResponse, BrokerError> {
    fetch_inner(scope, request, true)
}

fn fetch_inner(
    scope: &NetFetchScope,
    request: &BrokerRequest,
    allow_private: bool,
) -> Result<BrokerResponse, BrokerError> {
    let method = request.method.trim().to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "HEAD") {
        return Err(BrokerError::Invalid(format!(
            "method {method:?} is not supported by the phase-2 broker"
        )));
    }
    if !scope
        .methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&method))
    {
        return Err(BrokerError::Denied(format!(
            "method {method:?} is outside the granted net.fetch scope"
        )));
    }
    for name in request.headers.keys() {
        let lowered = name.to_ascii_lowercase();
        if !ALLOWED_HEADERS.contains(&lowered.as_str()) {
            return Err(BrokerError::Invalid(format!(
                "header {name:?} is not allowed through net.fetch"
            )));
        }
    }
    if request.headers.len() > 16 {
        return Err(BrokerError::Invalid(
            "net.fetch accepts at most 16 request headers".to_string(),
        ));
    }
    for (name, value) in &request.headers {
        if value.len() > 2048 || value.contains(['\r', '\n', '\0']) {
            return Err(BrokerError::Invalid(format!(
                "header {name:?} has an invalid value"
            )));
        }
    }

    let mut url = parse_url(&request.url)?;
    let mut redirects = 0u8;
    loop {
        ensure_host_allowed(&url, &scope.hosts)?;
        // Resolve once per hop, then give the HTTP client only those vetted
        // addresses. Keep the original URL for Host and TLS/SNI verification.
        let addresses = resolve_destination(&url, allow_private)?;
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .try_proxy_from_env(false)
            .resolver(move |_: &str| Ok(addresses.clone()))
            .timeout(Duration::from_secs(scope.timeout_secs))
            .build();

        let mut call = match method.as_str() {
            "HEAD" => agent.head(url.as_str()),
            _ => agent.get(url.as_str()),
        };
        for (name, value) in &request.headers {
            call = call.set(name, value);
        }
        // Avoid transparent compression changing the byte accounting.
        call = call.set("Accept-Encoding", "identity");

        let response = match call.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(error)) => {
                let text = error.to_string();
                return Err(
                    if text.to_ascii_lowercase().contains("timed out")
                        || text.to_ascii_lowercase().contains("timeout")
                    {
                        BrokerError::Timeout
                    } else {
                        BrokerError::Upstream(format!("upstream request failed: {text}"))
                    },
                );
            }
        };

        let status = response.status();
        if (300..400).contains(&status) {
            let location = response.header("location").map(str::to_string);
            let Some(location) = location else {
                // A 3xx without Location is just an upstream response.
                return read_response(response, scope.max_bytes);
            };
            if redirects >= scope.redirects {
                return Err(BrokerError::Redirect(format!(
                    "redirect limit {} reached",
                    scope.redirects
                )));
            }
            url = url
                .join(&location)
                .map_err(|error| BrokerError::Redirect(format!("invalid redirect: {error}")))?;
            url = parse_url(url.as_str())?;
            redirects += 1;
            continue;
        }

        return read_response(response, scope.max_bytes);
    }
}

fn read_response(response: ureq::Response, max_bytes: u64) -> Result<BrokerResponse, BrokerError> {
    let status = response.status();
    let content_type = response.header("content-type").map(str::to_string);
    let mut body = Vec::new();
    response
        .into_reader()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|error| {
            BrokerError::Upstream(format!("upstream body could not be read: {error}"))
        })?;
    if body.len() as u64 > max_bytes {
        return Err(BrokerError::TooLarge(max_bytes));
    }
    Ok(BrokerResponse {
        status,
        content_type,
        body,
    })
}

fn parse_url(raw: &str) -> Result<Url, BrokerError> {
    let mut url =
        Url::parse(raw).map_err(|error| BrokerError::Invalid(format!("invalid URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(BrokerError::Invalid(
            "only http(s) URLs can be fetched".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BrokerError::Invalid(
            "URLs with userinfo are not accepted".to_string(),
        ));
    }
    if url.host_str().is_none() {
        return Err(BrokerError::Invalid("URL has no host".to_string()));
    }
    url.set_fragment(None);
    Ok(url)
}

fn ensure_host_allowed(url: &Url, patterns: &[String]) -> Result<(), BrokerError> {
    let host = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| BrokerError::Invalid("URL has no host".to_string()))?;
    if host_matches(&host, patterns) {
        Ok(())
    } else {
        Err(BrokerError::Denied(format!(
            "host {host:?} is outside the granted net.fetch allowlist"
        )))
    }
}

/// Exact match, or `*.suffix` for a subdomain (but not the apex).
pub(crate) fn host_matches(host: &str, patterns: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    patterns.iter().any(|pattern| {
        let pattern = pattern.to_ascii_lowercase();
        match pattern.strip_prefix("*.") {
            Some(suffix) => host.ends_with(&format!(".{suffix}")) && host != suffix,
            None => host == pattern,
        }
    })
}

fn resolve_destination(
    url: &Url,
    allow_private: bool,
) -> Result<Vec<std::net::SocketAddr>, BrokerError> {
    let host = url
        .host_str()
        .ok_or_else(|| BrokerError::Invalid("URL has no host".to_string()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| BrokerError::Invalid("URL has no port".to_string()))?;

    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return if allow_private || is_public_ip(ip) {
            Ok(vec![std::net::SocketAddr::new(ip, port)])
        } else {
            Err(BrokerError::Denied(format!(
                "destination {ip} is not a public address"
            )))
        };
    }

    let addresses: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|error| BrokerError::Upstream(format!("cannot resolve {host}: {error}")))?
        .collect();
    if addresses.is_empty() {
        return Err(BrokerError::Upstream(format!(
            "cannot resolve {host}: no addresses"
        )));
    }
    for address in &addresses {
        if !allow_private && !is_public_ip(address.ip()) {
            return Err(BrokerError::Denied(format!(
                "destination {} is not a public address",
                address.ip()
            )));
        }
    }
    Ok(addresses)
}

/// Reject loopback, private, link-local, multicast, documentation, CGNAT and
/// other non-routable destinations. IPv4-mapped IPv6 is unwrapped first.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return is_public_v4(mapped);
            }
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local())
        }
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        || octets[0] == 0
        || octets[0] >= 240
        // 100.64.0.0/10 carrier-grade NAT.
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        // 192.0.0.0/24 protocol assignments.
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        // 198.18.0.0/15 benchmarking.
        || (octets[0] == 198 && (18..=19).contains(&octets[1])))
}

/// Redact a URL for the audit trail: host and path only, never the query.
pub fn redact_url(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(url) => {
            let host = url.host_str().unwrap_or("<invalid>");
            let path = url.path();
            if path.is_empty() || path == "/" {
                host.to_string()
            } else {
                format!("{host}{path}")
            }
        }
        Err(_) => "<invalid-url>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn spawn_server(response: Vec<u8>) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        });
        (address, handle)
    }

    #[test]
    fn private_destinations_are_rejected_before_connecting() {
        for raw in ["http://127.0.0.1/", "http://[::1]/", "http://10.0.0.1/"] {
            assert!(resolve_destination(&Url::parse(raw).unwrap(), false).is_err());
        }
        assert_eq!(
            resolve_destination(&Url::parse("https://8.8.8.8/").unwrap(), false).unwrap(),
            vec!["8.8.8.8:443".parse().unwrap()]
        );
    }

    #[test]
    fn redirects_recheck_destination_allowlist_before_connecting() {
        let (address, server) = spawn_server(b"HTTP/1.1 302 Found\r\nLocation: http://unapproved.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec());
        let mut scope = loopback_scope(address, 1024);
        scope.redirects = 1;
        let result =
            fetch_loopback_for_test(&scope, &BrokerRequest::get(format!("http://{address}/")));
        assert!(matches!(result, Err(BrokerError::Denied(_))));
        server.join().unwrap();
    }

    #[test]
    fn pinned_resolver_connects_without_resolving_url_hostname() {
        let (address, server) = spawn_server(http_response("200 OK", b"pinned"));
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .resolver(move |_: &str| Ok(vec![address]))
            .timeout(Duration::from_secs(2))
            .build();
        // .invalid cannot resolve through normal DNS. The URL remains intact
        // while the connector uses only the supplied validated address.
        assert_eq!(
            agent
                .get("http://unresolvable.invalid/")
                .call()
                .unwrap()
                .into_string()
                .unwrap(),
            "pinned"
        );
        server.join().unwrap();
    }

    fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn loopback_scope(address: std::net::SocketAddr, max_bytes: u64) -> NetFetchScope {
        NetFetchScope {
            hosts: vec![address.ip().to_string()],
            methods: vec!["GET".into()],
            max_bytes,
            timeout_secs: 5,
            redirects: 0,
            rate_per_min: 60,
        }
    }

    fn scope(hosts: &[&str]) -> NetFetchScope {
        NetFetchScope {
            hosts: hosts.iter().map(|host| (*host).to_string()).collect(),
            methods: vec!["GET".into(), "HEAD".into()],
            max_bytes: 1024,
            timeout_secs: 5,
            redirects: 0,
            rate_per_min: 60,
        }
    }

    #[test]
    fn blocks_private_and_special_addresses() {
        for text in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = text.parse().unwrap();
            assert!(!is_public_ip(ip), "{text} must not be public");
        }
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn host_allowlist_supports_exact_and_wildcard_entries() {
        let patterns = vec![
            "api.example.com".to_string(),
            "*.cdn.example.com".to_string(),
        ];
        assert!(
            ensure_host_allowed(&Url::parse("https://api.example.com/x").unwrap(), &patterns)
                .is_ok()
        );
        assert!(ensure_host_allowed(
            &Url::parse("https://a.cdn.example.com/x").unwrap(),
            &patterns
        )
        .is_ok());
        assert!(
            ensure_host_allowed(&Url::parse("https://cdn.example.com/x").unwrap(), &patterns)
                .is_err()
        );
        assert!(
            ensure_host_allowed(&Url::parse("https://evil.example/x").unwrap(), &patterns).is_err()
        );
    }

    #[test]
    fn rejects_userinfo_and_non_http_urls() {
        assert!(parse_url("ftp://example.com/x").is_err());
        assert!(parse_url("https://user:pass@example.com/x").is_err());
        assert!(parse_url("https://example.com/x#frag").is_ok());
    }

    #[test]
    fn rejects_headers_outside_the_allowlist() {
        let mut request = BrokerRequest::get("https://api.example.com/x");
        request.headers.insert("cookie".into(), "secret".into());
        let error = fetch(&scope(&["api.example.com"]), &request).unwrap_err();
        assert!(matches!(error, BrokerError::Invalid(_)));
    }

    #[test]
    fn rejects_methods_outside_the_scope() {
        let mut request = BrokerRequest::get("https://api.example.com/x");
        request.method = "POST".into();
        let error = fetch(&scope(&["api.example.com"]), &request).unwrap_err();
        assert!(matches!(error, BrokerError::Invalid(_)));
    }

    #[test]
    fn redacts_query_parameters() {
        assert_eq!(
            redact_url("https://api.example.com/vod?token=secret&wd=x"),
            "api.example.com/vod"
        );
        assert_eq!(redact_url("not a url"), "<invalid-url>");
    }

    #[test]
    fn requests_are_constructed_without_a_raw_url_in_the_error() {
        // The scope check happens before DNS or a socket, so this is
        // deterministic and does not touch the network.
        let request = BrokerRequest::get("https://evil.example/x");
        let error = fetch(&scope(&["api.example.com"]), &request).unwrap_err();
        assert!(matches!(error, BrokerError::Denied(_)));
        assert!(!error.message().contains("token"));
    }

    #[test]
    fn broker_scope_defaults_match_the_capability_schema() {
        let value = serde_json::from_value::<NetFetchScope>(json!({
            "hosts": ["api.example.com"]
        }))
        .unwrap();
        assert_eq!(value.methods, vec!["GET"]);
        assert_eq!(value.max_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn fetches_through_the_broker_with_a_loopback_fixture() {
        let (address, server) = spawn_server(http_response("200 OK", b"hello"));
        let scope = loopback_scope(address, 1024);
        let reply = fetch_loopback_for_test(
            &scope,
            &BrokerRequest::get(format!("http://{address}/vod?token=secret")),
        )
        .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type.as_deref(), Some("text/plain"));
        assert_eq!(reply.body, b"hello");
        server.join().unwrap();
    }

    #[test]
    fn a_response_over_the_scope_limit_is_rejected() {
        let (address, server) = spawn_server(http_response("200 OK", &vec![b'x'; 2048]));
        let scope = loopback_scope(address, 1024);
        let error =
            fetch_loopback_for_test(&scope, &BrokerRequest::get(format!("http://{address}/big")))
                .unwrap_err();
        assert!(matches!(error, BrokerError::TooLarge(1024)), "{error:?}");
        server.join().unwrap();
    }

    #[test]
    fn a_redirect_is_refused_when_the_scope_forbids_it() {
        let (address, server) = spawn_server(
            b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        );
        let scope = loopback_scope(address, 1024);
        let error = fetch_loopback_for_test(
            &scope,
            &BrokerRequest::get(format!("http://{address}/start")),
        )
        .unwrap_err();
        assert!(matches!(error, BrokerError::Redirect(_)), "{error:?}");
        server.join().unwrap();
    }
}
