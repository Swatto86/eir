# Agent standards mirror

Canonical home: private repo `Swatto86/agent-standards` → synced to `~/.agents` via `sync.ps1`.

This directory is a **portable copy** of standards written during cloud-agent sessions when the private repo is unreachable. After pulling Eir, copy files into `~/.agents` and run sync, or merge into agent-standards on GitHub.

| File | Purpose |
|------|---------|
| `rust-build.md` | Rust compile-speed defaults (dev profiles, sccache, fastcheck) |

## Roll out to other Rust repos

1. Read `rust-build.md`.
2. Apply the same workspace `Cargo.toml`, `.cargo/config.toml`, and CI sccache blocks.
3. Add a project-specific `fastcheck` script (PowerShell on Windows-first repos, shell elsewhere).

**WattMail:** ready-to-apply patch at `docs/patches/wattmail-compile-speed.patch`:

```bash
cd /path/to/WattMail
git checkout main
git am /path/to/eir/docs/patches/wattmail-compile-speed.patch
git push origin main
```
