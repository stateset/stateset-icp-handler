#!/usr/bin/env node
// Entry point for `npx @stateset/icp-conformance`. Resolves the Rust
// reference binary, then forwards all argv + exit code through.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  execBinary,
  missingBinaryMessage,
  resolveBinary,
} from "../src/resolve.js";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(here, "..", "package.json"), "utf8"));

const resolved = resolveBinary({ packageVersion: pkg.version });

if (resolved.kind === "missing") {
  process.stderr.write(missingBinaryMessage() + "\n");
  process.exit(127);
}

const argv = process.argv.slice(2);
execBinary(resolved, argv);
