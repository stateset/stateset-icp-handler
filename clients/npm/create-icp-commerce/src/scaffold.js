// Core scaffolder. Kept separate from the CLI entry point so it can be
// exercised directly by unit tests without spawning a subprocess.

import { fileURLToPath } from "node:url";
import { dirname, join, resolve, sep } from "node:path";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";

const here = dirname(fileURLToPath(import.meta.url));
const TEMPLATE_DIR = join(here, "..", "template");

// Cargo package names: ASCII letters, digits, `_`, `-`; can't start
// with a digit. Don't allow path separators or shell-special chars
// since the name is also used as the directory name.
const NAME_RE = /^[a-zA-Z][a-zA-Z0-9_-]{0,63}$/;

export class ScaffoldError extends Error {
  constructor(message) {
    super(message);
    this.name = "ScaffoldError";
  }
}

export function validateName(name) {
  if (typeof name !== "string" || name.length === 0) {
    throw new ScaffoldError(
      "missing project name.\nUsage: create-icp-commerce <name>",
    );
  }
  if (name.includes("/") || name.includes("\\") || name.includes(sep)) {
    throw new ScaffoldError(
      `project name "${name}" may not contain a path separator`,
    );
  }
  if (!NAME_RE.test(name)) {
    throw new ScaffoldError(
      `project name "${name}" is not a valid Cargo package name. ` +
        `Use ASCII letters, digits, hyphens, or underscores (start with a letter).`,
    );
  }
  return name;
}

// Every template file → the destination file it maps to. Keeping this
// explicit (rather than a walk) makes it obvious what lands in a
// generated project and keeps any future template/ additions
// deliberate.
const FILES = [
  { src: "Cargo.toml.tmpl", dst: "Cargo.toml" },
  { src: "src/main.rs.tmpl", dst: "src/main.rs" },
  { src: "env.tmpl", dst: ".env" },
  { src: "gitignore.tmpl", dst: ".gitignore" },
  { src: "README.md.tmpl", dst: "README.md" },
];

function substitute(body, vars) {
  return body.replace(/\{\{(\w+)\}\}/g, (_, key) => {
    if (!(key in vars)) {
      throw new ScaffoldError(`template references unknown variable {{${key}}}`);
    }
    return vars[key];
  });
}

export function scaffold({ name, targetDir, cwd = process.cwd() }) {
  validateName(name);
  const target = resolve(cwd, targetDir ?? name);

  if (existsSync(target)) {
    throw new ScaffoldError(
      `refusing to overwrite existing path: ${target}\n` +
        `delete it first or pick a different name.`,
    );
  }

  mkdirSync(target, { recursive: true });
  const written = [];
  const vars = { name };

  for (const { src, dst } of FILES) {
    const tmplPath = join(TEMPLATE_DIR, src);
    const body = readFileSync(tmplPath, "utf8");
    const rendered = substitute(body, vars);
    const outPath = join(target, dst);
    mkdirSync(dirname(outPath), { recursive: true });
    writeFileSync(outPath, rendered);
    written.push(outPath);
  }

  return { target, written };
}

export function nextSteps({ name }) {
  return [
    "",
    "\x1b[32m✓\x1b[0m Scaffolded " + name + "/",
    "",
    "  cd " + name,
    "  cargo run --release",
    "",
    "  # in another terminal",
    "  curl -s http://localhost:8082/.well-known/icp | jq '{tier: .conformance.tier}'",
    "",
    "  # validate conformance",
    "  npx @stateset/icp-conformance --url http://localhost:8082 \\",
    "       --api-key icp_demo_key_123 \\",
    "       --agent-id did:stateset:agent:conformance",
    "",
    "See " + name + "/README.md for a Python buy-flow example.",
    "",
  ].join("\n");
}
