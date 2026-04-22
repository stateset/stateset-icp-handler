# Changelog — `@stateset/icp-conformance` (npm)

Release notes for the npm wrapper only. Changes to the underlying Rust
`icp-conformance` binary (what the wrapper actually executes) live in
the [monorepo changelog](../../../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

_No changes yet._

## [0.2.0] — 2026-04-21

Initial public release. Paired with `stateset-icp-handler` 0.2.0.

### Added
- **`npx @stateset/icp-conformance`** — one-command conformance tester
  for any ICP handler URL. Wraps the reference Rust binary
  (`icp-conformance`, shipped in the monorepo as a cargo bin) so
  non-Rust developers can validate handlers against the spec without
  installing cargo.
- **Binary resolution, in priority order:**
  1. `ICP_CONFORMANCE_BIN` environment variable (explicit override —
     useful for CI pinning a specific build of the reference binary).
  2. Platform-specific cache at
     `~/.cache/stateset/icp-conformance/<version>/icp-conformance`.
     Reserved for a future `postinstall` hook that downloads a
     pre-built binary from GitHub releases; the cache is read but
     never written by this version.
  3. `icp-conformance` on `PATH` (from
     `cargo install stateset-icp-handler`).
  4. `cargo run --bin icp-conformance --` fallback, discovered by
     walking up from `cwd` looking for a Cargo.toml that declares the
     binary.
- **Clean failure mode** — if no resolution path succeeds, prints a
  single actionable message naming all four install paths and exits
  POSIX 127.
- **Pure Node.js** — zero runtime dependencies, ESM, Node 18+.
- **`node --test` coverage** (4 tests) of the resolver logic:
  `ICP_CONFORMANCE_BIN` honored when the path exists; raises when
  that path doesn't exist; the all-misses case correctly resolves to
  `{ kind: "missing" }`; the missing-binary message names the three
  install paths.
