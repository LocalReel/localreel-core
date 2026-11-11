# localreel-core

The LocalReel server: a single Rust binary that indexes local media, serves the
web UI built into it, and runs scan/probe/thumbnail/transcode work through a
persistent job queue.

## Features

- **Library indexing** — add local folders as sources; incremental scans
  fingerprint files, record scan history, and mark missing files without
  deleting their state.
- **Metadata and thumbnails** — `ffprobe`/`ffmpeg` extract duration, codecs,
  dimensions and JPEG thumbnails through typed operations.
- **Playback policy** — direct streaming (HTTP Range, ETag, HEAD) for browser
  codecs; H.264 in MKV/MOV/TS/FLV/AVI is remuxed (stream copy) instead of
  re-encoded; other sources are transcoded to cached variants.
- **Series model** — the scanner derives series, season and episode numbers from
  the library layout and release-style file names.
- **Ranked feed** — `GET /api/v1/feed` orders continue-watching first, then
  unwatched episodes by series, then movies, then finished items.
- **Persistent jobs** — scan, probe, thumbnail, transcode, cache GC and backup
  restore run in `sys_jobs` with leases, retries, progress, cancellation and
  crash recovery.
- **State and backup** — per-file playback position, likes and favourites;
  versioned backup export/import; legacy import from a LocalReel v1 database.
- **Accounts** — one owner account that manages the household, plus viewer
  accounts; watch progress, likes and favourites are per account; passwords
  are Argon2id hashes, sessions live in the database and writes carry a CSRF
  token.
- **Two personas** — `localreel serve` decides whether it was started by a
  person (opens the web UI, heals a busy port) or by a service manager
  (fixed port, no window); `--mode` and `LOCALREEL_MODE` override it.
- **Local by design** — binds loopback only by default; a non-loopback bind
  needs an explicit opt-in, and every request then needs a signed-in account.

## Workspace

```text
crates/localreel-format/   versioned backup/artifact schema (no I/O)
crates/localreel-kernel/   generic capabilities: config, db, jobs, events,
                           storage, media-ops (typed ffmpeg)
crates/localreel-media/    media domain: sources, scan, metadata, state,
                           search, playback policy, variants, shares, audit
crates/localreel-plugin/   plugin host: signed packages, capabilities, sandbox
crates/localreel-update/   signed update manifest, verified download, installer
                           hand-off (two editions)
crates/localreel-api/      HTTP /api/v1, SSE events, static assets, loopback guard
crates/localreel-server/   the `localreel` binary: wiring, migrations, serve
docs/adr/                  architecture decision records
```

Dependency direction is one-way:

```text
web -> api -> media -> kernel
```

The kernel does not know about videos; the media crate does not write HTTP
handlers or spawn `ffmpeg` directly.

## Commands

```powershell
# development server (isolated: port 3101, data in .dev-data/)
.\scripts\dev.ps1

cargo run -p localreel-server -- serve
cargo run -p localreel-server -- migrate
cargo run -p localreel-server -- doctor

# walk/fingerprint an already configured source
cargo run -p localreel-server -- scan --source-id 1

# import a copy of a LocalReel v1 database
cargo run -p localreel-server -- import C:\backup\localreel.db --dry-run
cargo run -p localreel-server -- import C:\backup\localreel.db
```

Configuration comes from `localreel.toml` (or `LOCALREEL_CONFIG`) and then
`LOCALREEL_*` environment variables; see `localreel.toml.example`. Default bind
is `127.0.0.1:3001`; data lives in `./data` unless `LOCALREEL_DATA_DIR` is set.

A development checkout and an installed release stay apart: the dev script uses
port 3101 and `.dev-data/`, an installed app uses 3001 and its install folder.
See [docs/development.md](docs/development.md).

## Quality gates

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same gates on Windows and Ubuntu.

## Where this fits

This repository is a **component**: it builds the server binary. Installers,
downloads and the composed product live in the
[`LocalReel`](https://github.com/LocalReel/LocalReel) product repository. See
[docs/adr/](docs/adr/) for the architecture decisions behind the boundaries.

## License

Apache-2.0 — see [`LICENSE`](LICENSE). Copyright 2026 LocalReel contributors.

`ffmpeg` is not bundled by default: the server discovers `ffmpeg`/`ffprobe` on
`PATH`, next to the binary, in `ffmpeg/bin`, or via `LOCALREEL_FFMPEG_DIR`. A
product package can ship a build (the MSI/zip layouts then contain
`ffmpeg/bin` plus its license and notice); any ffmpeg you install or bundle
keeps its own license.

---

## 简体中文

LocalReel 的服务器组件：一个 Rust 单文件程序，索引本地媒体、内置 Web UI，并通过
持久化任务队列执行扫描、探测、缩略图与转码。

- **媒体库索引**：把本地目录添加为来源；增量扫描记录文件指纹、扫描历史，文件
  缺失时只标记、不删除状态。
- **元数据与缩略图**：通过 `ffprobe`/`ffmpeg` 提取时长、编码、分辨率与缩略图。
- **播放策略**：浏览器可直接播放的格式走直连（支持 Range、ETag、HEAD）；MKV、
  MOV、TS、FLV、AVI 中的 H.264 用流拷贝封装成 MP4；其余格式转码为缓存副本。
- **剧集模型**：扫描时从目录结构与发布式文件名推导系列、季、集数。
- **排序 feed**：`GET /api/v1/feed` 按「续播 → 未看剧集 → 未看电影 → 已看完」
  排序。
- **持久化任务**：扫描、探测、缩略图、转码、缓存回收与备份恢复都走 `sys_jobs`，
  支持租约、重试、进度、取消与崩溃恢复。
- **播放状态与备份**：按文件保存进度、喜欢与收藏；支持版本化备份导入导出，以及
  从 LocalReel v1 数据库导入。
- **账户**：一个所有者账户管理家庭里的其他账户；观看进度、喜欢与收藏按账户分开；
  密码用 Argon2id 哈希，会话存数据库，写操作需带 CSRF 令牌。
- **两种角色**：`localreel serve` 自行判断是「人启动的」（自动打开界面、端口被占
  时自动顺延）还是「服务管理器启动的」（固定端口、不开窗口）；可用 `--mode` 与
  `LOCALREEL_MODE` 覆盖。
- **本地优先**：默认只监听回环地址；绑定非回环地址需要显式开启，且此后每个请求都
  需要已登录的账户。

构建与测试：`cargo fmt --all -- --check`、`cargo clippy --workspace
--all-targets -- -D warnings`、`cargo test --workspace`。

本仓库是组件，产物（安装包、下载）在产品仓库
[`LocalReel`](https://github.com/LocalReel/LocalReel)。
