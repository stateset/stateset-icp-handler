// Unit tests for the scaffolder. Hermetic — every test runs against a
// fresh temp directory so parallel runs and reruns don't collide.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, it } from "node:test";

import {
  scaffold,
  nextSteps,
  validateName,
  ScaffoldError,
} from "../src/scaffold.js";

let work;
beforeEach(() => {
  work = mkdtempSync(join(tmpdir(), "create-icp-"));
});
afterEach(() => {
  rmSync(work, { recursive: true, force: true });
});

describe("validateName", () => {
  it("accepts typical cargo-style names", () => {
    for (const ok of ["my-store", "my_store", "store1", "A", "agent42-beta"]) {
      assert.equal(validateName(ok), ok);
    }
  });

  it("rejects empty / undefined", () => {
    assert.throws(() => validateName(""), /missing project name/);
    assert.throws(() => validateName(undefined), /missing project name/);
  });

  it("rejects names containing path separators", () => {
    assert.throws(() => validateName("a/b"), /path separator/);
    assert.throws(() => validateName("../escape"), /path separator/);
  });

  it("rejects names that don't match cargo package rules", () => {
    assert.throws(() => validateName("1store"), /Cargo/);
    assert.throws(() => validateName("my store"), /Cargo/);
    assert.throws(() => validateName("my.store"), /Cargo/);
  });
});

describe("scaffold", () => {
  it("writes every template file with {{name}} substituted", () => {
    const { target, written } = scaffold({ name: "my-store", cwd: work });

    assert.equal(target, join(work, "my-store"));
    assert.equal(written.length, 5);

    const cargo = readFileSync(join(target, "Cargo.toml"), "utf8");
    assert.match(cargo, /name = "my-store"/);
    assert.doesNotMatch(cargo, /\{\{/);

    const main = readFileSync(join(target, "src/main.rs"), "utf8");
    assert.match(main, /my-store — a StateSet ICP merchant/);
    assert.doesNotMatch(main, /\{\{/);

    const env = readFileSync(join(target, ".env"), "utf8");
    assert.match(env, /ICP_STATE_DB_PATH=\.\/icp-state\.db/);

    const gitignore = readFileSync(join(target, ".gitignore"), "utf8");
    assert.match(gitignore, /\/target/);

    const readme = readFileSync(join(target, "README.md"), "utf8");
    assert.match(readme, /^# my-store/);
  });

  it("refuses to overwrite an existing directory", () => {
    scaffold({ name: "first", cwd: work });
    assert.throws(
      () => scaffold({ name: "first", cwd: work }),
      /refusing to overwrite/,
    );
  });

  it("writes a targetDir override when provided", () => {
    const { target } = scaffold({
      name: "my-store",
      targetDir: "custom-dir",
      cwd: work,
    });
    assert.equal(target, join(work, "custom-dir"));
    assert.ok(existsSync(join(target, "Cargo.toml")));
  });

  it("throws ScaffoldError (not generic Error) on validation failure", () => {
    let caught;
    try {
      scaffold({ name: "bad name", cwd: work });
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof ScaffoldError);
  });
});

describe("nextSteps", () => {
  it("mentions the project name and next commands", () => {
    const out = nextSteps({ name: "my-store" });
    assert.match(out, /my-store/);
    assert.match(out, /cargo run --release/);
    assert.match(out, /icp-conformance/);
  });
});
