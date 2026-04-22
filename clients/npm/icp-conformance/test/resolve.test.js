// Unit tests for the binary-resolution logic. We exercise the three
// concrete outcomes (override, missing, cache) without needing the
// Rust binary to actually exist — keeps these tests hermetic and fast.
//
// Run with: `node --test test/`

import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, it } from "node:test";

import {
  missingBinaryMessage,
  resolveBinary,
} from "../src/resolve.js";

const VERSION = "test";

// Save and scrub env vars that affect resolution so each test starts
// from a known baseline.
const savedEnv = {};
const SCRUB_KEYS = ["ICP_CONFORMANCE_BIN", "PATH", "XDG_CACHE_HOME", "HOME"];

beforeEach(() => {
  for (const k of SCRUB_KEYS) savedEnv[k] = process.env[k];
});

afterEach(() => {
  for (const k of SCRUB_KEYS) {
    if (savedEnv[k] === undefined) delete process.env[k];
    else process.env[k] = savedEnv[k];
  }
});

describe("resolveBinary", () => {
  it("honors ICP_CONFORMANCE_BIN when the path exists", () => {
    // Any existing file works — resolver only checks existence, not
    // executability (spawnSync handles the run-time failure).
    const dir = mkdtempSync(join(tmpdir(), "icp-resolve-"));
    const fake = join(dir, "fake-bin");
    writeFileSync(fake, "");
    process.env.ICP_CONFORMANCE_BIN = fake;
    const r = resolveBinary({ packageVersion: VERSION });
    assert.equal(r.kind, "override");
    assert.equal(r.command, fake);
    assert.deepEqual(r.args, []);
  });

  it("throws when ICP_CONFORMANCE_BIN points nowhere", () => {
    process.env.ICP_CONFORMANCE_BIN = join(tmpdir(), "definitely-not-there");
    assert.throws(
      () => resolveBinary({ packageVersion: VERSION }),
      /does not exist/,
    );
  });

  it("returns kind=missing when override/cache/PATH/cargo all fail", () => {
    delete process.env.ICP_CONFORMANCE_BIN;
    // Empty PATH: no binary discoverable.
    process.env.PATH = "";
    // Isolated cache dir — empty, so cache lookup misses.
    const cacheDir = mkdtempSync(join(tmpdir(), "icp-cache-"));
    process.env.XDG_CACHE_HOME = cacheDir;
    // HOME points at an empty dir so the platform-default cache
    // path (~/.cache/…) also misses even if XDG_CACHE_HOME isn't
    // honored on this platform.
    process.env.HOME = mkdtempSync(join(tmpdir(), "icp-home-"));
    mkdirSync(join(process.env.HOME, "Library", "Caches"), { recursive: true });
    const r = resolveBinary({ packageVersion: VERSION });
    assert.equal(r.kind, "missing");
  });
});

describe("missingBinaryMessage", () => {
  it("names the three install paths", () => {
    const msg = missingBinaryMessage();
    assert.match(msg, /cargo install/);
    assert.match(msg, /github\.com\/stateset/);
    assert.match(msg, /ICP_CONFORMANCE_BIN/);
  });
});
