//! Build-time embedded web assets.
//!
//! `build.rs` walks `localreel-web/dist` and generates a `FILES` table with
//! `include_bytes!` entries, so a release binary can serve the UI without a
//! separate `dist` directory. If the UI was never built, `FILES` is empty and
//! the API falls back to the disk loader / placeholder page.

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

pub fn is_embedded() -> bool {
    !FILES.is_empty()
}

pub fn get(path: &str) -> Option<(&'static [u8], String)> {
    let path = path.trim_start_matches('/');
    FILES.iter().find_map(|(candidate, bytes)| {
        if *candidate == path {
            Some((
                *bytes,
                mime_guess::from_path(candidate)
                    .first_or_octet_stream()
                    .to_string(),
            ))
        } else {
            None
        }
    })
}

pub fn index() -> Option<(&'static [u8], String)> {
    get("index.html")
}
