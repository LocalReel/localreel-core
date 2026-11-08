//! A minimal collector that proves the broker path end to end.
//!
//! It asks for `net.fetch` in its manifest, calls the host broker through
//! [`BrokerClient`], parses a MacCMS-shaped response into normalized titles and
//! writes `collected.json` into its working directory. It never opens an
//! outbound socket itself; the host's grant, scope, SSRF policy and audit are
//! the only way out.
//!
//! This is deliberately not a CMS: there is no catalog table yet. It is the
//! first consumer that makes the broker contract real, and the fixture tests
//! pin the parser before the data model is committed.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use localreel_plugin::broker_client::BrokerClient;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct CollectedTitle {
    remote_id: String,
    name: String,
    lines: Vec<CollectedLine>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct CollectedLine {
    source_tag: String,
    episodes: Vec<CollectedEpisode>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct CollectedEpisode {
    index: usize,
    name: String,
    play_url: String,
}

/// Parse the `list` array of a MacCMS-style detail response.
///
/// The real adapter will add validation, posters, downloads and persistence.
/// This slice only needs the identity and the play lines to prove the broker.
fn parse_catalog(value: &Value) -> Vec<CollectedTitle> {
    let Some(items) = value.get("list").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let remote_id = item.get("vod_id").and_then(scalar_text)?;
            let name = item.get("vod_name").and_then(Value::as_str)?.trim();
            if name.is_empty() {
                return None;
            }
            let from = item
                .get("vod_play_from")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let urls = item
                .get("vod_play_url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let lines = parse_lines(from, urls);
            if lines.is_empty() {
                return None;
            }
            Some(CollectedTitle {
                remote_id,
                name: name.to_string(),
                lines,
            })
        })
        .collect()
}

fn parse_lines(from: &str, urls: &str) -> Vec<CollectedLine> {
    let tags: Vec<&str> = from.split("$$$").collect();
    urls.split("$$$")
        .enumerate()
        .filter_map(|(line_index, line)| {
            let source_tag = tags
                .get(line_index)
                .map(|tag| tag.trim())
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("line-{}", line_index + 1));
            let episodes: Vec<CollectedEpisode> = line
                .split('#')
                .enumerate()
                .filter_map(|(episode_index, entry)| {
                    let (label, url) = match entry.split_once('$') {
                        Some((label, url)) => (label.trim(), url.trim()),
                        None => ("", entry.trim()),
                    };
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        return None;
                    }
                    let name = if label.is_empty() {
                        format!("第{}集", episode_index + 1)
                    } else {
                        label.to_string()
                    };
                    Some(CollectedEpisode {
                        index: episode_index + 1,
                        name,
                        play_url: url.to_string(),
                    })
                })
                .collect();
            if episodes.is_empty() {
                None
            } else {
                Some(CollectedLine {
                    source_tag,
                    episodes,
                })
            }
        })
        .collect()
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn collect() {
    let config: Value = std::env::var("LOCALREEL_PLUGIN_CONFIG")
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    let Some(url) = config.get("url").and_then(Value::as_str) else {
        eprintln!("collector: no url in LOCALREEL_PLUGIN_CONFIG");
        return;
    };
    let Some(client) = BrokerClient::from_env() else {
        eprintln!("collector: LOCALREEL_PLUGIN_HOST_URL is missing");
        return;
    };
    let value: Value = match client.fetch_json(url, &[("accept", "application/json")]) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("collector: broker fetch failed: {error}");
            return;
        }
    };
    let titles = parse_catalog(&value);
    let episodes: usize = titles
        .iter()
        .flat_map(|title| &title.lines)
        .map(|line| line.episodes.len())
        .sum();
    let out = config
        .get("out")
        .and_then(Value::as_str)
        .unwrap_or("collected.json");
    match serde_json::to_vec_pretty(&titles) {
        Ok(bytes) => {
            if let Err(error) = std::fs::write(out, bytes) {
                eprintln!("collector: cannot write {out}: {error}");
            } else {
                eprintln!(
                    "collector: titles={} episodes={} wrote={out}",
                    titles.len(),
                    episodes
                );
            }
        }
        Err(error) => eprintln!("collector: cannot encode result: {error}"),
    }

    if config
        .get("catalog")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        match client.write_catalog(&titles) {
            Ok(written) => eprintln!("collector: catalog drafts={written}"),
            Err(error) => eprintln!("collector: catalog.write failed: {error}"),
        }
    }
}

fn main() {
    let token = std::env::var("LOCALREEL_PLUGIN_TOKEN").unwrap_or_default();
    std::thread::spawn(|| {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 64];
        while matches!(stdin.read(&mut buffer), Ok(read) if read > 0) {}
        std::process::exit(0);
    });

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    println!("{}", json!({ "ready": true, "port": port }));
    std::io::stdout().flush().ok();

    // Fetch after the handshake so a slow upstream never blocks startup.
    std::thread::spawn(collect);

    for stream in listener.incoming().flatten() {
        let _ = handle(stream, &token);
    }
}

fn handle(mut stream: TcpStream, token: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut authorized = token.is_empty();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("x-localreel-token") && value.trim() == token {
                authorized = true;
            }
        }
    }
    if !authorized {
        return respond(&mut stream, 403, b"forbidden");
    }
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let (path, _query) = target.split_once('?').unwrap_or((&target, ""));
    match path {
        "/v1/health" => respond(&mut stream, 200, b"{\"ok\":true}"),
        _ => respond(&mut stream, 404, b"not found"),
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_and_episodes() {
        let value = json!({
            "list": [{
                "vod_id": 1001,
                "vod_name": "示例影片",
                "vod_play_from": "量子$$$非凡",
                "vod_play_url": "正片$https://cdn.example/1001.m3u8#预告$https://cdn.example/1001-trailer.m3u8$$$正片$https://cdn2.example/1001.m3u8"
            }]
        });
        let titles = parse_catalog(&value);
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0].remote_id, "1001");
        assert_eq!(titles[0].lines.len(), 2);
        assert_eq!(titles[0].lines[0].source_tag, "量子");
        assert_eq!(titles[0].lines[0].episodes.len(), 2);
        assert_eq!(titles[0].lines[0].episodes[0].name, "正片");
        assert_eq!(
            titles[0].lines[0].episodes[0].play_url,
            "https://cdn.example/1001.m3u8"
        );
        assert_eq!(titles[0].lines[1].source_tag, "非凡");
        assert_eq!(titles[0].lines[1].episodes.len(), 1);
    }

    #[test]
    fn drops_entries_without_an_id_or_playable_url() {
        let value = json!({
            "list": [
                {"vod_id": "", "vod_name": "no id"},
                {"vod_id": 2, "vod_name": "no url", "vod_play_url": "正片$not-a-url"},
                {"vod_id": 3, "vod_name": "ok", "vod_play_url": "正片$https://x/y.m3u8"}
            ]
        });
        let titles = parse_catalog(&value);
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0].remote_id, "3");
    }

    #[test]
    fn missing_names_are_numbered() {
        let lines = parse_lines("量子", "$https://x/y.m3u8");
        assert_eq!(lines[0].episodes[0].name, "第1集");
    }
}
