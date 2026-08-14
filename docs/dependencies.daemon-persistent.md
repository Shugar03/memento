# Dependencies — daemon-persistent additions

> Parallel file created during `daemon-persistent` B8 because
> `docs/dependencies.md` was under active user edits (WIP). Merge this
> section into `docs/dependencies.md` once it is free.

## daemon-persistent deps

The `daemon-persistent` change (REQ-DAEMON-003/006/012) adds two Windows-only
workspace dependencies to the pinning policy:

- **`interprocess`** — the named-pipe transport (tokio duplex) shared by the
  daemon accept loop, the CLI pipe client and the MCP stdio→pipe proxy.
- **`windows`** — Job Objects (`KILL_ON_JOB_CLOSE | BREAKAWAY_OK`, REQ-DAEMON-003
  spawn guard), owner-only security descriptors on the pipe and cookie
  (REQ-DAEMON-012), and `FILE_SHARE_NONE` for the `.daemon-spawn.lock`
  (design D6). The crate is already in the tree via `sysinfo` 0.39, so the
  exact `0.62.2` lock version adds zero new transitive builds.

## Pin table (verified 2026-08-14)

| Name | Version / source | Why pinned / chosen | Upgrade path |
|---|---|---|---|
| `interprocess` | `"2.4.3"`, feature `["tokio"]` | Windows named pipes with tokio duplex (REQ-DAEMON-006 4 KB-buffer framing). MSRV 1.75 < toolchain 1.95 (design D2). `windows-sys 0.61` already in `Cargo.lock` via `sysinfo` — zero new transitive deps. | bump the version in `Cargo.toml` → `[workspace.dependencies]`; verify `cargo check --workspace`; run `cargo test -p memento-mcp -j 2` + `cargo test -p memento-cli -j 2`; update this table; commit together |
| `windows` | `"0.62.2"`, features `Win32_System_JobObjects`, `Win32_System_Threading`, `Win32_Foundation`, `Win32_Security`, `Win32_Storage_FileSystem` | Job Objects (REQ-DAEMON-003 R1 startup guard), owner-only DACL on pipe + cookie (REQ-DAEMON-012), `CreateFileW(FILE_SHARE_NONE)` spawn lock (design D6). Already resolved in `Cargo.lock` via `sysinfo 0.39` (0.62.2) — zero extra build. | bump the version in `Cargo.toml`; verify the Job Objects + DACL paths via `cargo test -p memento-cli -j 2` (spawn tests) and `cargo test -p memento-mcp -j 2`; update this table; commit together |

## Windows-gnu build note

Local checks run on `x86_64-pc-windows-gnu` (see `docs/dependencies.md` §
"Environment note (Windows local builds)"); `interprocess` and `windows`
compile cleanly there and are exercised by the daemon pipe integration tests.
