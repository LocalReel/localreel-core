use localreel_kernel::migrations::Migration;

/// Media-domain migrations. The server applies these after the kernel stream.
pub fn list() -> &'static [Migration] {
    &[
        Migration {
            version: 1,
            name: "media",
            sql: include_str!("../migrations/0001_media.sql"),
        },
        Migration {
            version: 2,
            name: "media_sources_archived",
            sql: include_str!("../migrations/0002_sources_archived.sql"),
        },
        Migration {
            version: 3,
            name: "media_series",
            sql: include_str!("../migrations/0003_series.sql"),
        },
        Migration {
            version: 4,
            name: "media_series_rev",
            sql: include_str!("../migrations/0004_series_rev.sql"),
        },
        Migration {
            version: 5,
            name: "accounts",
            sql: include_str!("../migrations/0005_accounts.sql"),
        },
        Migration {
            version: 6,
            name: "media_tracks",
            sql: include_str!("../migrations/0006_tracks.sql"),
        },
    ]
}
