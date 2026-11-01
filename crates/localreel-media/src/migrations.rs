use localreel_kernel::migrations::Migration;

/// Media-domain migrations. The server applies these after the kernel stream.
pub fn list() -> &'static [Migration] {
    &[Migration {
        version: 1,
        name: "media",
        sql: include_str!("../migrations/0001_media.sql"),
    }]
}
