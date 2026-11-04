//! WebDAV storage provider for LocalReel.
//!
//! The plugin speaks the sidecar protocol (see `localreel_plugin::server`) and
//! maps `PROPFIND`/`GET` against a WebDAV base URL to the host's storage
//! contract. URIs are the full HTTP(S) URLs of the objects, which is how the
//! host stores remote media.

use std::io::Read;
use std::time::Duration;

use base64::Engine as _;
use localreel_plugin::server::{Entry, Handler, HandlerError, Stat};
use serde_json::Value;

pub struct WebDavConfig {
    /// Base URL of the collection exposed to LocalReel (no trailing slash).
    pub base_url: String,
    pub username: String,
    pub password: String,
}

pub struct WebDavClient {
    agent: ureq::Agent,
    base_url: String,
    authorization: String,
}

impl WebDavClient {
    pub fn new(config: WebDavConfig) -> Self {
        let base_url = config.base_url.trim_end_matches('/').to_string();
        let authorization = if config.username.is_empty() {
            String::new()
        } else {
            let raw = format!("{}:{}", config.username, config.password);
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(raw)
            )
        };
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(120))
            .build();
        Self {
            agent,
            base_url,
            authorization,
        }
    }

    fn ensure_under_base(&self, url: &str) -> Result<(), HandlerError> {
        if url == self.base_url || url.starts_with(&format!("{}/", self.base_url)) {
            Ok(())
        } else {
            Err(HandlerError::failed(format!(
                "uri is outside the configured WebDAV base: {url}"
            )))
        }
    }

    fn request(&self, method: &str, url: &str) -> ureq::Request {
        let mut request = self.agent.request(method, url);
        if !self.authorization.is_empty() {
            request = request.set("Authorization", &self.authorization);
        }
        request
    }

    fn propfind(&self, url: &str, depth: &str) -> Result<Vec<DavEntry>, HandlerError> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;
        let response = self
            .request("PROPFIND", url)
            .set("Depth", depth)
            .set("Content-Type", "application/xml; charset=utf-8")
            .send_string(body);
        match response {
            Ok(response) => {
                let mut body = String::new();
                let mut reader = response.into_reader().take(32 * 1024 * 1024);
                reader.read_to_string(&mut body).map_err(|error| {
                    HandlerError::failed(format!("cannot read PROPFIND: {error}"))
                })?;
                Ok(parse_multistatus(&body))
            }
            Err(ureq::Error::Status(404, _)) => Err(HandlerError::NotFound),
            Err(ureq::Error::Status(code, _)) => Err(HandlerError::failed(format!(
                "PROPFIND {url} returned HTTP {code}"
            ))),
            Err(error) => Err(HandlerError::failed(format!(
                "PROPFIND {url} failed: {error}"
            ))),
        }
    }

    pub fn stat(&self, url: &str) -> Result<Stat, HandlerError> {
        self.ensure_under_base(url)?;
        let entries = self.propfind(url, "0")?;
        let entry = entries.into_iter().next().ok_or(HandlerError::NotFound)?;
        Ok(Stat {
            size: entry.size,
            modified: entry.modified,
            is_dir: entry.is_dir,
        })
    }

    pub fn list(&self, url: &str, recursive: bool) -> Result<Vec<Entry>, HandlerError> {
        self.ensure_under_base(url)?;
        let depth = if recursive { "infinity" } else { "1" };
        let mut entries = self.propfind(url, depth)?;
        let requested = url.trim_end_matches('/');
        entries.retain(|entry| {
            let resolved = resolve_href(url, &entry.href).unwrap_or_default();
            let resolved = resolved.trim_end_matches('/');
            !same_resource(resolved, requested)
        });
        entries
            .into_iter()
            .map(|entry| to_entry(url, entry))
            .collect()
    }

    pub fn read(&self, url: &str, start: u64, len: u64) -> Result<Vec<u8>, HandlerError> {
        self.ensure_under_base(url)?;
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(len).saturating_sub(1);
        let response = self
            .request("GET", url)
            .set("Range", &format!("bytes={start}-{end}"))
            .call();
        match response {
            Ok(response) => {
                let status = response.status();
                let mut reader = response.into_reader();
                let mut bytes = Vec::new();
                if status == 206 {
                    reader.take(len).read_to_end(&mut bytes).map_err(|error| {
                        HandlerError::failed(format!("cannot read body: {error}"))
                    })?;
                } else {
                    // Server ignored Range: skip to the requested offset.
                    std::io::copy(&mut (&mut reader).take(start), &mut std::io::sink()).map_err(
                        |error| HandlerError::failed(format!("cannot skip body: {error}")),
                    )?;
                    reader.take(len).read_to_end(&mut bytes).map_err(|error| {
                        HandlerError::failed(format!("cannot read body: {error}"))
                    })?;
                }
                Ok(bytes)
            }
            Err(ureq::Error::Status(404, _)) => Err(HandlerError::NotFound),
            Err(ureq::Error::Status(code, _)) => Err(HandlerError::failed(format!(
                "GET {url} returned HTTP {code}"
            ))),
            Err(error) => Err(HandlerError::failed(format!("GET {url} failed: {error}"))),
        }
    }
}

fn to_entry(base: &str, entry: DavEntry) -> Result<Entry, HandlerError> {
    let uri = resolve_href(base, &entry.href)?;
    let name = resource_name(&uri);
    Ok(Entry {
        uri,
        name,
        is_dir: entry.is_dir,
        size: entry.size,
        modified: entry.modified,
    })
}

fn resource_name(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    decode_href(last)
}

fn same_resource(href: &str, url: &str) -> bool {
    normalize(href).trim_end_matches('/') == normalize(url).trim_end_matches('/')
}

fn normalize(value: &str) -> String {
    let decoded = decode_href(value);
    decoded.trim_end_matches('/').to_string()
}

fn decode_href(value: &str) -> String {
    localreel_plugin::server::percent_decode(value)
}

/// Turn an href from a multistatus response into an absolute URL.
pub fn resolve_href(base: &str, href: &str) -> Result<String, HandlerError> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Ok(href.to_string());
    }
    let origin = match base.find("://") {
        Some(index) => {
            let rest = &base[index + 3..];
            match rest.find('/') {
                Some(slash) => &base[..index + 3 + slash],
                None => base,
            }
        }
        None => return Err(HandlerError::failed(format!("invalid base url: {base}"))),
    };
    if href.starts_with('/') {
        return Ok(format!("{origin}{href}"));
    }
    // Relative href: resolve against the base collection.
    let directory = base.trim_end_matches('/');
    Ok(format!("{directory}/{href}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavEntry {
    pub href: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<i64>,
}

/// Parse a WebDAV `multistatus` body.
///
/// The parser is deliberately small: it walks tags, tracks the current
/// `response` scope and captures `href`, `resourcetype/collection`,
/// `getcontentlength` and `getlastmodified`. Namespace prefixes are ignored.
pub fn parse_multistatus(xml: &str) -> Vec<DavEntry> {
    let mut entries = Vec::new();
    let mut current: Option<DavEntry> = None;
    let mut capture: Option<Capture> = None;
    let mut text: Vec<u8> = Vec::new();
    let bytes = xml.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'<' {
            if capture.is_some() {
                text.push(bytes[index]);
            }
            index += 1;
            continue;
        }
        let Some(end) = xml[index..].find('>') else {
            break;
        };
        let raw_tag = &xml[index + 1..index + end];
        index += end + 1;
        if raw_tag.starts_with('?') || raw_tag.starts_with('!') {
            continue;
        }
        let closing = raw_tag.starts_with('/');
        let self_closing = raw_tag.ends_with('/');
        let name_part = raw_tag
            .trim_start_matches('/')
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or_default();
        let local = local_name(name_part);

        if closing {
            if let Some(capture) = capture.take() {
                finish_capture(&mut current, capture, &text);
                text.clear();
            }
            if local == "response" {
                if let Some(entry) = current.take() {
                    if !entry.href.is_empty() {
                        entries.push(entry);
                    }
                }
            }
            continue;
        }

        if local == "response" {
            current = Some(DavEntry {
                href: String::new(),
                is_dir: false,
                size: 0,
                modified: None,
            });
        }
        if let Some(active) = capture {
            finish_capture(&mut current, active, &text);
            text.clear();
        }
        match local.as_str() {
            "href" => capture = Some(Capture::Href),
            "getcontentlength" => capture = Some(Capture::Length),
            "getlastmodified" => capture = Some(Capture::Modified),
            "collection" => {
                if let Some(entry) = current.as_mut() {
                    entry.is_dir = true;
                }
            }
            _ => {}
        }
        if self_closing && capture.is_some() {
            capture = None;
        }
    }
    entries
}

#[derive(Clone, Copy)]
enum Capture {
    Href,
    Length,
    Modified,
}

fn finish_capture(current: &mut Option<DavEntry>, capture: Capture, text: &[u8]) {
    let Some(entry) = current.as_mut() else {
        return;
    };
    let value = String::from_utf8_lossy(text);
    let value = value.trim();
    match capture {
        Capture::Href => entry.href = value.to_string(),
        Capture::Length => entry.size = value.parse().unwrap_or(0),
        Capture::Modified => entry.modified = parse_http_date(value),
    }
}

fn local_name(tag: &str) -> String {
    tag.rsplit(':').next().unwrap_or(tag).to_ascii_lowercase()
}

fn parse_http_date(value: &str) -> Option<i64> {
    httpdate::parse_http_date(value)
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
}

/// Handler exposed to the LocalReel host.
pub struct WebDavHandler {
    client: WebDavClient,
}

impl WebDavHandler {
    pub fn new(config: WebDavConfig) -> Self {
        Self {
            client: WebDavClient::new(config),
        }
    }
}

impl Handler for WebDavHandler {
    fn stat(&self, uri: &str) -> Result<Stat, HandlerError> {
        self.client.stat(uri)
    }

    fn list(&self, uri: &str, recursive: bool) -> Result<Vec<Entry>, HandlerError> {
        self.client.list(uri, recursive)
    }

    fn read(&self, uri: &str, start: u64, len: u64) -> Result<Vec<u8>, HandlerError> {
        self.client.read(uri, start, len)
    }
}

/// Build the handler from the JSON passed in `LOCALREEL_PLUGIN_CONFIG`.
pub fn handler_from_config(config: &Value) -> Result<WebDavHandler, String> {
    let base_url = config
        .get("base_url")
        .and_then(Value::as_str)
        .ok_or_else(|| "config.base_url is required".to_string())?;
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err("config.base_url must be an http(s) URL".into());
    }
    Ok(WebDavHandler::new(WebDavConfig {
        base_url: base_url.to_string(),
        username: config
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        password: config
            .get("password")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/library/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/library/Show%20A/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/library/Show%20A/ep01.mp4</d:href>
    <d:propstat><d:prop>
      <d:resourcetype/>
      <d:getcontentlength>12345</d:getcontentlength>
      <d:getlastmodified>Wed, 01 Nov 2023 10:00:00 GMT</d:getlastmodified>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn parses_a_multistatus_body() {
        let entries = parse_multistatus(MULTISTATUS);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_dir);
        assert!(entries[1].is_dir);
        assert_eq!(entries[1].href, "/dav/library/Show%20A/");
        assert!(!entries[2].is_dir);
        assert_eq!(entries[2].size, 12345);
        assert_eq!(entries[2].modified, Some(1698832800));
    }

    #[test]
    fn resolves_hrefs_to_absolute_urls() {
        let base = "https://nas.local/dav/library";
        assert_eq!(
            resolve_href(base, "/dav/library/a.mp4").unwrap(),
            "https://nas.local/dav/library/a.mp4"
        );
        assert_eq!(
            resolve_href(base, "https://other.test/x.mp4").unwrap(),
            "https://other.test/x.mp4"
        );
        assert_eq!(
            resolve_href(base, "Show%20A/ep01.mp4").unwrap(),
            "https://nas.local/dav/library/Show%20A/ep01.mp4"
        );
    }

    #[test]
    fn rejects_uris_outside_the_base() {
        let client = WebDavClient::new(WebDavConfig {
            base_url: "https://nas.local/dav/library".into(),
            username: String::new(),
            password: String::new(),
        });
        assert!(client.stat("https://evil.test/a.mp4").is_err());
        assert!(client.stat("http://nas.local/dav/library/a.mp4").is_err());
    }

    #[test]
    fn builds_basic_authorization() {
        let client = WebDavClient::new(WebDavConfig {
            base_url: "http://nas.local/dav".into(),
            username: "reader".into(),
            password: "secret".into(),
        });
        assert_eq!(
            client.authorization,
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("reader:secret")
            )
        );
    }

    #[test]
    fn config_requires_an_http_base() {
        assert!(handler_from_config(&serde_json::json!({})).is_err());
        assert!(handler_from_config(&serde_json::json!({"base_url": "ftp://x"})).is_err());
        assert!(handler_from_config(&serde_json::json!({"base_url": "http://x/dav"})).is_ok());
    }
}
