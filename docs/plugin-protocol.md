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
- `entry.runtime` (optional) names an interpreter that runs the program, for
  example `"node"` or `"python3"`. The host then starts
  `<runtime> <program> <args...>`; without it the program itself is executed.
  Runtime names are bare command names resolved through `PATH`; path separators
  are rejected. This is how script plugins avoid shipping a launcher binary;
  `localreel-plugin-cli new --lang node|python` generates such projects.
- `permissions` may contain `storage:read` and `network`; unknown or duplicate
  entries are rejected. A `storage` plugin always has `storage:read` — it is
  implied by the kind, so a package that predates the permission keeps loading
  and `effective_permissions()` reports it anyway. `network` is never implied.

### Capabilities (phase 1)

A manifest may also request typed capabilities. The name and scope schema are
owned by the host; the plugin chooses only from the published vocabulary:

```json
{
  "capabilities": [
    {
      "name": "net.fetch",
      "scope": {
        "hosts": ["api.example.com"],
        "methods": ["GET"],
        "max_bytes": 16777216,
        "timeout_secs": 30,
        "redirects": 0,
        "rate_per_min": 60
      }
    },
    {
      "name": "catalog.write",
      "scope": { "mode": "draft", "max_items_per_run": 2000 }
    }
  ]
}
```

- Unknown, duplicate, malformed and **forbidden** names are rejected before the
  package is extracted. The forbidden list includes `net.listen`, `net.socket`,
  `process.spawn`, `host.database`, `host.config`, `host.filesystem`,
  `plugin.install`, `ui.inject` and `audit.disable`.
- Phase 1 vocabulary: `fs.read`, `net.fetch`, `secret.use`, `schedule.request`,
  `catalog.write`, `blob.read`, `blob.write`. Tiers (0 implicit … 3 high) and
  scope bounds are host-side; `catalog.write` with `mode: "publish"` is tier 3.
- A grant is **per plugin version** and lives in `plugin_grants`. An upgrade that
  adds a capability needs a new grant. Administrators use:

  ```text
  GET    /api/v1/plugins/:id/capabilities
  POST   /api/v1/plugins/:id/capabilities          {"capability":"net.fetch","scope":{...}}
  DELETE /api/v1/plugins/:id/capabilities/:capability
  ```

  The response mixes `source: "manifest"` (declared) with `source: "legacy"`
  (the old `storage:read`/`network` permissions, shown for compatibility).
- **Enforcement status (phase 2.5):** `net.fetch`, `secret.use`,
  `catalog.write(draft)`, `schedule.request` and `blob.read`/`blob.write` are
  enforced by the host broker: the plugin's per-run token, the active grant for
  its exact version, the declared scope and the SSRF/redirect policy are checked
  before any side effect, and every allow/deny is audited. `fs.read` remains the
  legacy storage-root path; it is disclosed but not brokered. None of this is an
  OS sandbox: a plugin can still open its own socket, which is why phase 3
  replaces the transport and adds OS isolation (ADR-019).

## Broker: `net.fetch` (phase 2.1)

The plugin receives `LOCALREEL_PLUGIN_HOST_URL` and its per-run token. It calls:

```http
POST /api/v1/plugin-broker/fetch
x-localreel-plugin: <LOCALREEL_PLUGIN_TOKEN>
content-type: application/json

{"url": "https://api.example.com/vod?ac=detail&pg=1", "method": "GET",
 "headers": {"referer": "https://site/", "user-agent": "..."}}
```

The host:

1. authenticates the token;
2. finds the **active** `net.fetch` grant for this plugin id and version;
3. checks `hosts` (exact or `*.suffix`), `methods` (GET/HEAD), `max_bytes`,
   `timeout_secs`, `redirects` and `rate_per_min`;
4. resolves the destination and refuses loopback/private/link-local/multicast/
   documentation/CGNAT addresses, validating every redirect hop;
5. forwards only `accept`, `accept-language`, `user-agent` and `referer`;
   Cookie/Authorization belong to `secret.use`, not to this endpoint;
6. writes an audit row (`capability.fetch`, result, scope digest, bytes, duration,
   request id) and only then returns.

A successful response is a JSON envelope; the body is base64 so binary responses
survive:

```json
{"status": 200, "content_type": "application/json", "body_b64": "eyJ..."}
```

The plugin SDK offers `BrokerClient::from_env()`, `fetch_json()` and
`fetch_with_secret()`. A missing grant, an out-of-scope host/method, a forbidden
header or a non-public destination is a `403`; invalid input is `400`;
upstream/timeout conditions are `503`. The legacy `/api/v1/plugin-proxy` endpoint
remains for old storage plugins; new code should use the broker.

## Broker: `secret.use` (phase 2.2)

A plugin never receives a credential. It names a handle in the broker request:

```json
{"url": "https://api.example.com/vod", "method": "GET",
 "headers": {"accept": "application/json"}, "secret": "api-key"}
```

The manifest declares where the host may inject it:

```json
{
  "name": "secret.use",
  "scope": {
    "handles": ["api-key"],
    "hosts": ["api.example.com"],
    "inject": "header",
    "name": "X-Api-Key",
    "prefix": ""
  }
}
```

Admins store the value (never returned by any API) with:

```text
PUT    /api/v1/plugins/:id/secrets/:handle    {"value":"..."}
GET    /api/v1/plugins/:id/secrets            # handles and timestamps only
DELETE /api/v1/plugins/:id/secrets/:handle
```

Rules the host enforces:

- the `secret.use` grant must be active for the plugin's exact version;
- the handle and destination host must be in the granted scope;
- `inject` is `header` (default name `Authorization`, default prefix `Bearer `)
  or `query` (GET only); the plugin may not already set the same header/param;
- a request using a secret may not follow redirects, so the credential cannot be
  forwarded to a host outside the scope;
- the value is injected after the `net.fetch` scope check and before the socket;
  the audit row records the handle and host, never the value.

Storage caveat: values live in the local SQLite database protected by the OS
account boundary. Encryption at rest is a follow-up; the guarantee this phase
makes is that the **plugin never reads the value**, not that an attacker with
the host's data directory cannot.

## Broker: `catalog.write(draft)` (phase 2.3)

A collector submits normalized titles; it never writes the catalog tables that
the user browses:

```http
POST /api/v1/plugin-broker/catalog
x-localreel-plugin: <LOCALREEL_PLUGIN_TOKEN>
content-type: application/json

{"items": [{"remote_id": "1001", "name": "示例影片",
  "lines": [{"source_tag": "量子", "episodes": [
    {"index": 1, "name": "正片", "play_url": "https://cdn.example/1.m3u8"}
  ]}]}]}
```

The host checks the active `catalog.write` grant for the exact version, the
`mode`/`max_items_per_run` scope, validates every title/line/episode and upserts
`plugin_catalog_drafts`. Validation limits: 2000 items per run, 16 lines per
title, 500 episodes per line, 20 000 episodes per run, remote id ≤128 bytes,
name ≤512, source tag ≤64, episode name ≤256, http(s) play URL ≤2048 bytes.

Draft rows keep the original JSON, so the reviewed CMS promotion can add fields
without re-collecting. `mode: "publish"` is rejected at this layer: promotion
into `cms_*` is an admin action, not a plugin write. Admins inspect staged data
with `GET /api/v1/catalog/drafts`.

Play URLs in drafts are **untrusted plugin data**. They are not fetched by the
draft endpoint; the eventual playback/proxy path must apply its own URL policy
before opening a socket.

## Broker: `schedule.request` (phase 2.4)

A plugin asks the host for a periodic callback instead of starting its own
timer:

```http
POST /api/v1/plugin-broker/schedule
x-localreel-plugin: <LOCALREEL_PLUGIN_TOKEN>
content-type: application/json

{"interval_secs": 3600, "payload": {"page": 1}}
```

The manifest scope bounds the interval:

```json
{"name": "schedule.request",
 "scope": {"min_interval_secs": 3600, "max_interval_secs": 86400}}
```

The host checks the active grant for the exact version, stores one schedule per
plugin version, and every 15 seconds wakes due rows with
`GET /v1/schedule?payload=<url-encoded JSON>` against the plugin's loopback
port. The plugin's SDK `Handler::schedule` default returns 404; a collector can
implement it to run one collection. A failing callback still advances the next
run, so a broken plugin cannot create a hot retry loop. Run outcomes are audited
as `schedule.run` with the schedule id; the callback carries no browser session.

## Broker: `blob.read` / `blob.write` (phase 2.5)

A plugin gets a host-managed namespace instead of a writable directory:

```text
PUT    /api/v1/plugin-broker/blob/:name    raw bytes, blob.write
GET    /api/v1/plugin-broker/blob/:name    raw bytes, blob.read
DELETE /api/v1/plugin-broker/blob/:name    blob.write
```

The manifest scope supplies the total quota:

```json
{"name": "blob.write", "scope": {"quota_bytes": 104857600}}
```

Rules:

- the namespace is the plugin id; names are 1–128 bytes of
  `[A-Za-z0-9._-]` and may not contain `/`, `\` or `..`;
- one blob is at most 16 MiB; replacing a blob counts only the size delta;
- the total quota is checked before the write; over-quota is `400`;
- `blob.write` and `blob.read` are separate grants; deleting needs
  `blob.write`;
- metadata and usage are shown to admins at
  `GET /api/v1/plugins/:id/blobs`; blob bytes never appear in the audit row
  (only name and byte count).

Phase 2 stores blobs in SQLite. Moving them to files under the data directory
must keep the same quota/namespace checks.

### Reference collector (phase 2.1)

`plugin-collector-demo` is the first consumer that uses only the broker. Its
manifest asks for one allowlisted host:

```json
{
  "id": "collector-demo",
  "name": "Collector demo",
  "version": "0.1.0",
  "api_version": "localreel.plugin/v1",
  "kind": "storage",
  "entry": { "program": "plugin-collector-demo" },
  "capabilities": [
    {
      "name": "net.fetch",
      "scope": {
        "hosts": ["api.example.com"],
        "methods": ["GET"],
        "max_bytes": 16777216,
        "timeout_secs": 30,
        "redirects": 0,
        "rate_per_min": 60
      }
    },
    {
      "name": "catalog.write",
      "scope": { "mode": "draft", "max_items_per_run": 2000 }
    },
    {
      "name": "schedule.request",
      "scope": { "min_interval_secs": 3600, "max_interval_secs": 86400 }
    }
  ]
}
```

Its config names the request URL, output file and optional interval:

```json
{ "url": "https://api.example.com/api.php/provide/vod/?ac=detail&pg=1",
  "out": "collected.json", "catalog": true, "schedule_interval_secs": 3600 }
```

After the host grants `net.fetch`, `catalog.write` and `schedule.request`, the
plugin calls the broker, parses the MacCMS `list` into
`{remote_id,name,lines:[{source_tag,episodes}]}`, writes `collected.json`,
stages the same titles as catalog drafts and asks for a periodic callback. It
does not open an outbound socket itself. There is no CMS catalog yet; this slice
exists to keep the broker contract honest before the catalog schema is
committed.

The demo reuses `kind: "storage"` only because protocol v1 has no collector kind;
it serves health and nothing else, so it must not be pointed at a source. A
`kind: "catalog"` (or a collector capability) is a separate protocol decision
and is deliberately not smuggled into this slice.

## Package pinning (phase 3.1)

An operator can pin the exact artifact they approved:

```toml
[[plugins]]
package = "plugins/collector.lrplug"
sha256 = "64 lowercase or uppercase hex characters"
```

When `sha256` is present, the supervisor computes the whole-file SHA-256
**before inspecting or extracting** the package and refuses a mismatch as a
permanent failure. `localreel-plugin-pack` prints the digest, so the pin can be
copied from the build output.

This is artifact pinning, not a signature: it proves the host loaded the bytes
the operator approved, but it does not establish who built them. Publisher
signing is layered on top:

```toml
[[publishers]]
id = "acme"
public_key = "64 hex characters"

[[plugins]]
package = "plugins/collector.lrplug"
sha256 = "64 hex characters"
publisher = "acme"
signature = "128 hex characters"
```

`localreel-plugin-pack --signing-key publisher.seed` prints the public key and
the Ed25519 signature over the package SHA-256 digest; the seed file is 32 hex
bytes. When `publisher` is present the supervisor verifies the signature against
the trusted key **before** inspect/extract; a missing key, malformed signature
or mismatch is a permanent start failure.

The trust anchor is the configuration file: an attacker who can rewrite it can
also replace the publisher key. Key rotation is supported by listing several
keys with the same publisher id:

```toml
[[publishers]]
id = "acme"
public_key = "old key"
revoked = true

[[publishers]]
id = "acme"
public_key = "new key"
not_after = 1798761600
```

Only keys that are neither revoked nor past `not_after` verify a package; a
plugin whose publisher has no active key is rejected at config validation. This
is rotation/revocation by configuration, not a transparency log: there is no
append-only record of which key signed which release.

## Resource ceilings (phase 3.3)

```toml
[plugin_runtime]
max_memory_mb = 512
max_processes = 16
```

- Windows: both limits are set on the plugin's Job Object
  (`JOB_OBJECT_LIMIT_PROCESS_MEMORY` and `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`).
- Unix: `RLIMIT_AS` is applied before exec; the process-count limit is **not**
  enforced there yet (`RLIMIT_NPROC` is per-user, not per-tree).
- Defaults: 512 MiB and 16 processes; config validation bounds memory to
  16 MiB–16 GiB and processes to 1–256.

These limits do not block network access. A plugin can still open its own
socket; transport/OS isolation remains the next phase.

## Transports (phase 3.4)

`entry.transport` selects the control channel:

```json
{"entry": {"program": "bin/plugin", "transport": "stdio"}}
```

- `http` (default): the plugin listens on `127.0.0.1`, prints one ready line and
  serves the existing `/v1/*` endpoints.
- `stdio`: the plugin listens on nothing. The host writes one JSON request per
  line to stdin and reads one JSON response per line from stdout:

  ```json
  {"id":1,"token":"...","method":"stat","params":{"uri":"file:///a.mp4"}}
  {"id":1,"ok":true,"result":{"size":6,"modified":null,"is_dir":false}}
  ```

  Methods: `health`, `stat`, `list`, `walk`, `read` (returns
  `{"body_b64":"..."}`), `schedule`. Diagnostics go to stderr; stdout is the
  protocol. The SDK exposes `run_stdio(handler)`.

The environment tells the plugin which mode the host selected
(`LOCALREEL_PLUGIN_TRANSPORT`). Stdio removes the *need* to listen for a port;
it does not yet prevent a malicious plugin from opening its own socket. OS-level
network isolation remains phase 3 work.

## Permissions

The host is not an OS sandbox, and does not pretend to be one (ADR-014). What it
does is make the declaration matter:

| Permission | Effect |
|---|---|
| `storage:read` | Baseline for `kind = "storage"`. The plugin is handed its own source roots through `LOCALREEL_PLUGIN_ROOTS` and nothing else. |
| `network` | The host proxy (`POST /api/v1/plugin-proxy`) fetches URLs on the plugin's behalf. Without it the call is refused with `403` and recorded as `network.denied`. |

Enforcement, concretely:

- **Environment.** The sidecar is started with `env_clear()`. It inherits only
  `PATH`, `HOME`/`USERPROFILE`, `LANG`, `LC_ALL`, `TEMP`/`TMP`, and on Windows
  `SYSTEMROOT`/`WINDIR`. `LOCALREEL_DATA_DIR`, `LOCALREEL_CONFIG` and
  `LOCALREEL_LOG_DIR` are **not** passed: a plugin that could read them would not
  need a permission to reach the library.
- **Working directory.** `<data>/plugins/<id>/`, so relative writes stay in the
  plugin's own directory.
- **Roots.** `LOCALREEL_PLUGIN_ROOTS` is a JSON array of the source URIs this
  plugin owns (the rows in `sources` whose `kind` equals the plugin id). It is
  the only library location the plugin is told about.
- **Proxy.** `POST /api/v1/plugin-proxy` with
  `{"url": "https://..."}` and an `x-localreel-plugin: <LOCALREEL_PLUGIN_TOKEN>`
  header. The host checks the token, checks `network`, fetches with a 30 s
  timeout and a 32 MiB cap, and returns the response. `4xx`/`5xx` from the
  target are passed through.
- **Audit.** `start`, `network.allowed` and `network.denied` are written to
  `plugin_audit` (`GET /api/v1/plugins/audit`), and the admin page lists both the
  effective permissions and the recent activity.

A plugin can still open its own sockets: the host cannot stop that without an
OS-level sandbox, and claiming otherwise would be dishonest. What it can
guarantee is that every sanctioned request is attributable.
- Unknown manifest fields are ignored, never interpreted.

## Lifecycle

1. The host reads and validates `manifest.json` (`inspect`).
2. The host extracts the package into its own directory (`extract`).
3. The host starts the entry program with:
   - `LOCALREEL_PLUGIN_ID` — manifest id;
   - `LOCALREEL_PLUGIN_TOKEN` — random per-run token;
   - `LOCALREEL_PLUGIN_CONFIG` — JSON configuration (≤ 32 KiB);
   - `LOCALREEL_PLUGIN_ROOTS` — JSON array of the source roots the plugin owns;
   - `LOCALREEL_PLUGIN_HOST_URL` — base URL of the host broker
     (`http://127.0.0.1:<port>` in phase 2; phase 3 replaces the control channel).
4. With `transport = "http"` the plugin prints one line to stdout:
   `{"ready": true, "port": 12345}` and listens on `127.0.0.1`. With
   `transport = "stdio"` it prints no ready line and answers the host's health
   RPC on stdin/stdout instead.
5. The host calls health: `GET /v1/health` for HTTP, or the `health` JSON-RPC
   method for stdio, until it returns success (bounded retries).
6. On shutdown or when the host drops the runtime, the child process is killed.

The plugin must not write to stdout after the ready line (stdout is drained but
ignored); diagnostics belong on stderr, whose tail the host keeps for errors.

### When a plugin cannot start

A configured plugin that fails at any step above does **not** take the server
down. The entry stays in `GET /api/v1/plugins` with a state:

| State | Meaning |
|---|---|
| `starting` | The supervisor is extracting or waiting for the handshake. |
| `running` | The sidecar is up and its provider is registered. |
| `unhealthy` | The process is up but `GET /v1/health` keeps failing. |
| `failed` | Loading failed; `error` carries the reason (also recorded as a `start.failed` audit row). |
| `disabled` | `enabled = false` in the configuration; never loaded. |
| `stopped` | Stopped on purpose (admin action, or waiting to restart). |

Sources whose `kind` matches a configured but not-running plugin answer `503`
(`storage_unavailable`) instead of the generic `500`, so a client can tell
"temporarily down" from "this source is wrong".

Operators who prefer the old behaviour — refuse to start at all — set
`[plugin_runtime] strict = true`.

### Supervision

The host does not merely start a plugin once; it keeps it alive:

- **Crash detection.** The supervisor polls the child every 250 ms. A process
  that exits on its own is unregistered, reported as `failed` with its exit
  status, and retried after 1, 2, 4, 8 and 16 seconds. After five attempts the
  entry stays `failed` until an operator restarts it.
- **Permanent failures are not retried.** An unreadable package, an invalid
  manifest or a rejected API version is reported once; retrying cannot change
  it.
- **Health.** `GET /v1/health` runs every 10 seconds with a 2 s budget. Three
  consecutive failures mark the plugin `unhealthy`; six replace the process.
  Three health-driven restarts in a row give up.
- **Stops drain.** A stop stops accepting calls, waits up to
  `[plugin_runtime] stop_timeout_secs` (default 5) for in-flight requests,
  unregisters the provider, closes the child's stdin (the graceful path), and
  only then kills the process tree.
- **No orphans.** On Windows the child belongs to a Job Object with
  `KILL_ON_JOB_CLOSE`; on Unix it leads its own process group. A host crash
  takes the plugin down with it.

### Control API

Administrators can drive the lifecycle without editing `localreel.toml`:

| Request | Effect |
|---|---|
| `POST /api/v1/plugins/:id/restart` | Stop the current generation, start a new one. |
| `POST /api/v1/plugins/:id/disable?persist=false` | Stop it now; the configuration is untouched. |
| `POST /api/v1/plugins/:id/disable?persist=true` | Also write `enabled = false` into the matching `[[plugins]]` entry. |
| `POST /api/v1/plugins/:id/enable?persist=…` | The inverse. |
| `GET /api/v1/plugins/:id/logs?bytes=N` | The last N bytes of the plugin's stderr (≤ 64 KiB). |

Every transition publishes `plugin_state_changed` on the event stream and writes
an audit row (`start`, `stop`, `crash`, `start.failed`).

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