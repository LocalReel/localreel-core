# Sidecar plugin protocol

LocalReel can run storage providers as **sidecar processes**. The host validates a
package manifest, extracts the package, starts the entry program, and proxies
`Storage` calls over loopback HTTP. The kernel never loads plugin code into its
own address space.

This document describes the protocol implemented in
`localreel-core/crates/localreel-plugin`.

## Package layout

A plugin package is a zip file (`.lrplug`) whose root contains `manifest.json`
and the entry program:

```text
webdav.lrplug
├── manifest.json
├── bin/webdav-plugin        (or webdav-plugin.exe)
└── ...                      (assets the plugin needs)
```

Limits enforced before extraction or execution:

| Limit | Value |
|---|---|
| Package size | 64 MiB |
| Package entries | 4096 |
| Extracted size | 256 MiB |
| Manifest size | 64 KiB |

Extraction rejects absolute paths, `..` components, duplicate entries and
symlinks. Every file must stay inside the destination directory.

## Manifest

```json
{
  "id": "webdav",
  "name": "WebDAV storage",
  "version": "0.1.0",
  "api_version": "localreel.plugin/v1",
  "kind": "storage",
  "entry": { "program": "bin/webdav-plugin", "args": ["--serve"] },
  "permissions": ["network"],
  "description": "Optional one-line description"
}
```

Rules:

- `api_version` must equal `localreel.plugin/v1`; anything else is rejected
  before the package is extracted.
- `id` is 1–32 lowercase letters, digits or `-`, starting alphanumeric.
- `kind` is `storage`.
- `entry.program` is a relative path inside the package (no `..`, no drive
  prefix, no absolute path).
- `permissions` may contain `network`; unknown permissions are rejected. The
  host does not grant capabilities — the list documents what the plugin does.
- Unknown manifest fields are ignored, never interpreted.

## Lifecycle

1. The host reads and validates `manifest.json` (`inspect`).
2. The host extracts the package into its own directory (`extract`).
3. The host starts the entry program with:
   - `LOCALREEL_PLUGIN_ID` — manifest id;
   - `LOCALREEL_PLUGIN_TOKEN` — random per-run token;
   - `LOCALREEL_PLUGIN_CONFIG` — JSON configuration (≤ 32 KiB).
4. The plugin prints one line to stdout:
   `{"ready": true, "port": 12345}` and listens on `127.0.0.1`.
5. The host calls `GET /v1/health` until it returns 200 (bounded retries).
6. On shutdown or when the host drops the runtime, the child process is killed.

The plugin must not write to stdout after the ready line (stdout is drained but
ignored); diagnostics belong on stderr, whose tail the host keeps for errors.

## HTTP contract

Every request carries `X-LocalReel-Token: <token>`. A wrong or missing token
returns 403. The host only uses `GET`; responses use `Content-Length` and
`Connection: close`.

| Endpoint | Response |
|---|---|
| `GET /v1/health` | `{"ok": true}` |
| `GET /v1/stat?uri=<encoded>` | `{"size": 123, "modified": 1700000000, "is_dir": false}` |
| `GET /v1/list?uri=<encoded>` | `{"entries": [ … ]}` (direct children) |
| `GET /v1/walk?uri=<encoded>` | `{"entries": [ … ]}` (recursive) |
| `GET /v1/read?uri=<encoded>&start=N&len=M` | raw bytes, exactly `len` (or fewer at EOF) |

An entry is:

```json
{ "uri": "scheme://…", "name": "episode-01.mp4", "is_dir": false, "size": 42, "modified": 1700000000 }
```

Query values are percent-encoded UTF-8; `uri` is the same string the host sent
for `stat`/`list`/`read`. A missing object returns 404; other failures return a
5xx status and the host surfaces the request as a storage error.

`modified` is seconds since the Unix epoch; the scanner fingerprints on size and
mtime, so providers should return a value that changes when the object changes.

## Security properties

- **Manifest-first**: id, API version, entry point and permissions are parsed
  and validated before extraction or execution.
- **Explicit API version**: a newer plugin is rejected rather than
  best-effort executed.
- **No in-process code**: the plugin runs as a separate process with its own
  memory and permissions; a crash cannot take the host down.
- **Loopback + token**: the plugin port is bound to `127.0.0.1` and requires the
  per-run token, so other local users/processes cannot drive it accidentally.
- **Host-owned extraction directory**: package paths cannot escape it.
- **No host database access**: plugins receive no SQLite handle and no session.
  A storage plugin sees only the URIs the host asks about and the configuration
  it was started with.

## Host configuration

The server starts enabled plugins at boot and registers each provider under the
plugin id, which becomes the source `kind`:

```toml
[[plugins]]
package = "plugins/webdav.lrplug"
enabled = true

[plugins.config]
base_url = "https://dav.example.test/dav"
username = "reader"
```

The package is extracted under `<data>/plugins/<id>/<version>/`; the `config`
table is handed to the plugin as `LOCALREEL_PLUGIN_CONFIG`. A source then names
the plugin kind:

```http
POST /api/v1/sources
{"uri": "https://dav.example.test/dav/library", "kind": "webdav"}
```

Sources without a `kind` keep the local behaviour (existing directories are
canonicalized; other URI schemes map to their legacy kinds).
## Remote media

A storage provider may return URIs that are not local paths (for example
`http://plugin.local/library/ep01.mp4`). The host then:

- stores the URI as the media identity and derives series/episode information
  from its path;
- plays it through the provider (`stat` + `read`), exactly like a local file;
- probes and transcodes it through the server's own range-capable endpoint
  (`http://<host>:<port>/api/v1/media/<id>/stream`), so `ffprobe`/`ffmpeg` read
  the bytes on demand instead of downloading the file first.

The server's listen address is used for that endpoint; a bind on `0.0.0.0` is
reached through `127.0.0.1`. Jobs that need remote media fail with a clear
message when the server is not running (for example a one-off CLI command).
## Packing a plugin

```sh
cargo run -p localreel-plugin --bin localreel-plugin-pack -- \
  --dir path/to/plugin --out dist/webdav.lrplug
```

```text
packed webdav 0.1.0 -> dist/webdav.lrplug (2 files, 2338397 bytes)
sha256: e964cb071d1044272d2c8a41380b3240ea8f4eb311717ae89131f7f3bd6eeeea
```

The packer reads `manifest.json` first, validates it, requires the entry program
to exist inside the directory, rejects symlinks and non-relative paths, and
writes a deterministic archive: packing the same directory twice produces the
same SHA-256. `--force` replaces an existing output file.

## Tooling

The [localreel-plugin-cli](https://github.com/LocalReel/localreel-plugin-cli)
repository scaffolds a plugin project, validates manifests and packs `.lrplug`
files with a deterministic SHA-256. [localreel-plugin-strm](https://github.com/LocalReel/localreel-plugin-strm)
is a complete provider built with it: it exposes `.strm` files (text files that
contain a media URL) as remote media and proxies reads with HTTP Range.

## Reference plugins

Two sidecar plugins ship with the repository:

**`plugin-demo`** (`crates/localreel-plugin`) exposes one local directory. With
`public_http_base` set it reports HTTP URIs, which is how a remote provider looks
to the host; without it, entries are `file://` URIs.

**`webdav-plugin`** (`crates/localreel-plugin-webdav`) maps a WebDAV collection:

```toml
[[plugins]]
package = "plugins/webdav.lrplug"

[plugins.config]
base_url = "https://nas.local/dav/library"
username = "reader"
password = "secret"
```

A source then points at the same collection:

```http
POST /api/v1/sources
{"uri": "https://nas.local/dav/library", "kind": "webdav"}
```

The plugin uses `PROPFIND` (`Depth: 0`, `1` and `infinity`) for metadata and
listing, `GET` with `Range` for reads, and HTTP Basic authorization when a
username is configured. Entries are reported as the absolute HTTP(S) URLs of the
objects; the host keeps those URIs as media identities and reads bytes through
the plugin.