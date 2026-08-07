# CI — Memento RS

`.github/workflows/ci.yml` runs on push to `main` and on pull requests
against `main`.

## Jobs

| Job | Command | Purpose |
|---|---|---|
| `fmt` | `cargo fmt --all -- --check` | Formatting gate. |
| `clippy` | `cargo clippy --all-targets --all-features -- -D warnings` | Lint gate; also covers dev targets. |
| `test` | `cargo test --all` | Unit + integration tests across the workspace. |
| `bench-compile` | `cargo bench --no-run` | Benches must keep compiling (gates on criterion harness). |
| `audit` | `cargo audit` | Advisory DB scan (installed via `taiki-e/install-action`). |
| `geiger` | `cargo geiger --all-features` | Unsafe-code surface report (installed via `taiki-e/install-action`). |

All jobs use `actions/checkout@v4`, a pinned stable toolchain via
`dtolnay/rust-toolchain@stable`, and `Swatinem/rust-cache@v2` for shared
cargo caches. A `concurrency` group cancels superseded runs for the same ref.

## Local equivalents

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
cargo bench --no-run
cargo install cargo-audit && cargo audit
cargo install cargo-geiger && cargo geiger --all-features
```

The `audit` and `geiger` jobs need their tools installed (see
`scripts/audit-prep.sh` in batch 12 for the packaged equivalent).

## Cost notes (early batches)

- `bench-compile` rebuilds every dependency in the `bench` profile
  (opt-level 3) even with zero bench targets — it is the most expensive
  job until benches exist (batch 11). Kept as a compile gate; the local
  equivalent on the bootstrap host was not run to completion for this reason.
- `audit` / `geiger` are CI-only gates (tools not installed on the
  bootstrap host); local equivalents are `cargo install cargo-audit` /
  `cargo install cargo-geiger`.
