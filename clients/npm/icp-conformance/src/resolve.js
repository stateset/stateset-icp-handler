// Binary resolution for @stateset/icp-conformance.
//
// The user-facing contract is: `npx @stateset/icp-conformance --url …`
// Just Works™. Internally we prefer, in this order:
//
//   1. $ICP_CONFORMANCE_BIN            — explicit override (takes
//      precedence over everything; useful for CI pinning a specific
//      build of the reference binary).
//   2. ~/.cache/stateset/icp-conformance/<version>/icp-conformance
//      — cached pre-built binary. Reserved for a future `npm install`
//      post-install hook that pulls from a GitHub release. Not wired
//      yet; the cache directory is only read, never written.
//   3. PATH lookup for `icp-conformance`
//      — user installed via `cargo install` or from a distro package.
//   4. `cargo run --quiet --bin icp-conformance --`
//      — local monorepo checkout with Cargo on PATH. Discovered by
//      walking up from cwd looking for a Cargo.toml that declares this
//      binary.
//
// If none resolve we print a single actionable message covering all
// four install paths and exit non-zero. No network, no postinstall, no
// surprises.

import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const BIN_NAME = process.platform === "win32" ? "icp-conformance.exe" : "icp-conformance";

export function resolveBinary({ packageVersion }) {
  const override = process.env.ICP_CONFORMANCE_BIN;
  if (override) {
    if (!fs.existsSync(override)) {
      throw new Error(
        `ICP_CONFORMANCE_BIN=${override} was set but that path does not exist`,
      );
    }
    return { kind: "override", command: override, args: [] };
  }

  const cached = cachedBinaryPath(packageVersion);
  if (cached && fs.existsSync(cached)) {
    return { kind: "cache", command: cached, args: [] };
  }

  const onPath = findOnPath(BIN_NAME);
  if (onPath) {
    return { kind: "path", command: onPath, args: [] };
  }

  const cargoRoot = findCargoWorkspace(process.cwd());
  if (cargoRoot && hasCargo()) {
    return {
      kind: "cargo",
      command: "cargo",
      args: [
        "run",
        "--quiet",
        "--manifest-path",
        path.join(cargoRoot, "Cargo.toml"),
        "--bin",
        "icp-conformance",
        "--",
      ],
    };
  }

  return { kind: "missing" };
}

function cachedBinaryPath(version) {
  const base =
    process.env.XDG_CACHE_HOME ||
    (process.platform === "darwin"
      ? path.join(os.homedir(), "Library", "Caches")
      : path.join(os.homedir(), ".cache"));
  return path.join(base, "stateset", "icp-conformance", version, BIN_NAME);
}

function findOnPath(name) {
  const pathSep = process.platform === "win32" ? ";" : ":";
  const entries = (process.env.PATH || "").split(pathSep);
  for (const dir of entries) {
    if (!dir) continue;
    const candidate = path.join(dir, name);
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return candidate;
    } catch {
      // not here; keep looking
    }
  }
  return null;
}

function hasCargo() {
  try {
    execFileSync("cargo", ["--version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

// Walk up from `start` looking for a Cargo.toml that declares an
// `icp-conformance` binary. Stop at filesystem root. Conservative —
// only matches the StateSet handler's workspace layout, not any
// crate that happens to be named similarly.
function findCargoWorkspace(start) {
  let dir = path.resolve(start);
  while (true) {
    const candidate = path.join(dir, "Cargo.toml");
    if (fs.existsSync(candidate)) {
      try {
        const text = fs.readFileSync(candidate, "utf8");
        if (
          text.includes('name = "icp-conformance"') ||
          text.includes('name = "stateset-icp-handler"')
        ) {
          return dir;
        }
      } catch {
        // unreadable; keep walking
      }
    }
    const parent = path.dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
}

export function missingBinaryMessage() {
  return [
    "Could not locate the `icp-conformance` binary.",
    "",
    "Install one of these ways, then re-run:",
    "",
    "  1. cargo install stateset-icp-handler --bin icp-conformance",
    "  2. Clone https://github.com/stateset/stateset-icp-handler and run",
    "     `cargo build --release --bin icp-conformance`, then point",
    "     ICP_CONFORMANCE_BIN at the resulting binary.",
    "  3. (macOS/Linux only) Symlink an existing build onto your PATH.",
    "",
    "Set ICP_CONFORMANCE_BIN=/path/to/icp-conformance to skip discovery.",
  ].join("\n");
}

// Run the resolved binary with the given argv, forwarding stdio and
// returning the child's exit code. Exits the parent process with the
// same code.
export function execBinary(resolved, argv) {
  const args = [...resolved.args, ...argv];
  const result = spawnSync(resolved.command, args, {
    stdio: "inherit",
    env: process.env,
  });
  if (result.error) {
    process.stderr.write(`icp-conformance: failed to spawn: ${result.error.message}\n`);
    process.exit(1);
  }
  if (typeof result.status === "number") {
    process.exit(result.status);
  }
  if (result.signal) {
    process.stderr.write(`icp-conformance: killed by signal ${result.signal}\n`);
    process.exit(1);
  }
  process.exit(0);
}
