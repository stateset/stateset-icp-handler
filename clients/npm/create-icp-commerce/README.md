# @stateset/create-icp-commerce

Scaffold a StateSet ICP merchant in one command.

```bash
npx @stateset/create-icp-commerce my-store
cd my-store
cargo run --release
```

Produces a minimal Rust binary that pairs
[`stateset-icp-handler`](https://github.com/stateset/stateset-icp-handler)
with the embedded iCommerce engine, preconfigured for frictionless
local development. In one terminal you have a running handler; in
another you can hit it with `curl`, the Python client, or
`npx @stateset/icp-conformance`.

## What you get

```
my-store/
├── Cargo.toml          # depends on stateset-icp-handler via git
├── src/main.rs         # thin wrapper: Config::load → build_app_state → serve
├── .env                # demo keys, SQLite state, sensible dev defaults
├── .gitignore
└── README.md           # run steps, buy flow from Python, config reference
```

## Requirements

- **Node 18+** (to run the scaffolder itself — nothing runtime).
- **Rust toolchain** (to build the generated project). Install from
  [rustup.rs](https://rustup.rs) if you don't have it.
- **Network** on first `cargo build` — pulls `stateset-icp-handler`
  from GitHub. Subsequent builds are fully cached.

## CLI

```
Usage: create-icp-commerce <name>
```

`<name>` must be a valid Cargo package name: ASCII letters, digits,
hyphens, and underscores; must start with a letter; no path separators.

Exit codes: `0` success, `1` scaffold failure (e.g. directory already
exists), `2` usage error.

## Next steps after scaffolding

1. `cd <name>`
2. `cargo run --release` — first build is slow (~1–2 min), subsequent
   runs instant.
3. `curl -s http://localhost:8082/.well-known/icp | jq`
4. Run the Python buy-flow from the generated README.
5. Validate against the spec with
   `npx @stateset/icp-conformance --url http://localhost:8082 --api-key icp_demo_key_123 --agent-id did:stateset:agent:conformance`.

## License

MIT OR Apache-2.0.
