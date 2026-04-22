# Changelog — `@stateset/create-icp-commerce` (npm)

Release notes for the scaffolder only. Changes to the template's
runtime dependencies (the `stateset-icp-handler` crate it pulls in,
the generated Rust project's behavior) live in the [monorepo
changelog](../../../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

_No changes yet._

## [0.2.0] — 2026-04-21

Initial public release. Paired with `stateset-icp-handler` 0.2.0.

### Added
- **`npx @stateset/create-icp-commerce <name>`** — scaffolds a StateSet
  ICP merchant project in `./<name>/`. Single required argument;
  non-interactive.
- **Generated project contents** (five files):
  - `Cargo.toml` — depends on `stateset-icp-handler` via git, binary
    named after the project.
  - `src/main.rs` — 20-line wrapper that loads `.env`, configures
    tracing, and calls `stateset_icp_handler::{build_app_state, serve}`.
  - `.env` — preconfigured dev defaults: demo keys, SQLite state +
    engine paths, `ICP_VERIFY_MANDATE_SIGNATURES=false` for frictionless
    local testing (with a comment warning to flip it for production),
    ACP/UCP/MCP/A2A compat surfaces all enabled.
  - `.gitignore` — covers `/target`, SQLite `.db` / `.db-wal` / `.db-shm`,
    and `.env.local`.
  - `README.md` — walks through `cargo run`, a `curl` smoke test, a
    Python buy-flow example, `npx @stateset/icp-conformance` validation,
    and a configuration reference table.
- **Name validation** — ASCII letters / digits / `-` / `_`, must start
  with a letter, no path separators, max 64 chars. Matches Cargo package
  naming rules.
- **Refuses to overwrite existing paths** — safer than `rm -rf` surprises.
- **`ScaffoldError` exception class** — callers using
  `scaffold({ name, cwd })` as a library (not just via the CLI) can
  catch it specifically.
- **Pure Node.js** — zero runtime dependencies, ESM, Node 18+.
- **`node --test` coverage** (9 tests):
  - Name validation: accepts typical names, rejects empty, rejects
    path separators, rejects Cargo-invalid names.
  - Template rendering: all five files written with `{{name}}`
    substituted correctly and no orphan template markers.
  - Refuses overwrite of an existing directory.
  - `targetDir` override is honored.
  - `ScaffoldError` is the surfaced error type (not a generic `Error`).
  - `nextSteps({ name })` names the expected follow-up commands.
