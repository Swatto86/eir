# Rust compile speed

Apply these defaults to every Rust workspace unless a project doc explicitly overrides them.

## Workspace `Cargo.toml`

Add dev-profile dependency optimization at the workspace root (does not affect release):

```toml
[profile.dev.package."*"]
opt-level = 1
```

Workspace crates stay at dev `opt-level = 0`; dependencies compile once with minimal opt, which materially speeds `cargo check`, `cargo clippy`, and `cargo test` on large graphs (tokio, windows, tauri, etc.).

Do **not** change `[profile.release]` for compile speed unless the user asks for release-time tradeoffs (LTO, `codegen-units`, etc.).

## `.cargo/config.toml`

- Set `[build] jobs = 0` so Cargo uses all logical CPUs on CI and local machines.
- **Do not** hard-code `rustc-wrapper = "sccache"` in committed config — many dev machines lack sccache. Use environment variables instead.

Optional per-host overrides (document in project README, do not commit unless the whole team uses the linker):

| Host | Linker | Notes |
|------|--------|-------|
| Linux | `mold` or `lld` via `rustflags = ["-C", "link-arg=-fuse-ld=mold"]` | Safe on most pure-Rust Linux targets |
| macOS | `lld` | Usually safe |
| Windows MSVC + static CRT / Tauri | **Avoid** until validated | `+crt-static` and Tauri bundling may break with `lld-link` |

## CI (GitHub Actions)

Every Rust job should use **both**:

1. `swatinem/rust-cache@v2` — caches `target/` and registry metadata
2. `mozilla-actions/sccache-action@v0.0.10` with job env:

```yaml
env:
  RUSTC_WRAPPER: sccache
  SCCACHE_GHA_ENABLED: "true"
```

Install sccache after the Rust toolchain step and before rust-cache. Keep the pinned toolchain (`dtolnay/rust-toolchain@…` matching `rust-toolchain.toml`).

## Local iteration scripts

Provide a fast script (e.g. `scripts/fastcheck.ps1` or `scripts/fast-check.sh`) that:

- Always runs formatting (and JS syntax if applicable)
- **Full workspace**: clippy or check + clippy — project choice
- **Scoped** (`-Package` / `-p`): `cargo check --locked -p <crate> --all-targets` only

Stage generated bundle inputs (Tauri `beforeBuildCommand` artifacts) conditionally — only when the UI crate or full workspace needs them.

Full gate (`verify.ps1` / CI) stays unchanged: tests, release build, packaging smokes.

## Agent workflow

When editing Rust:

1. Prefer `fastcheck -Package <crate>` during iteration
2. Run full `verify` / CI-equivalent before commit
3. When adding a new Rust workspace, copy the dev profile block and CI sccache wiring in the same PR

## Measuring

Optional: `cargo build -Z timings` (nightly) or `sccache --show-stats` after CI to confirm cache hit rate. Not required for every change.
