#!/usr/bin/env node
// Entry point for `npx @stateset/create-icp-commerce <name>`.

import { nextSteps, scaffold, ScaffoldError } from "../src/scaffold.js";

function printUsageAndExit(code) {
  process.stderr.write(
    [
      "Usage: create-icp-commerce <name>",
      "",
      "Scaffolds a StateSet ICP merchant project in ./<name>/.",
      "",
      "Example:",
      "  npx @stateset/create-icp-commerce my-store",
      "  cd my-store && cargo run --release",
      "",
    ].join("\n"),
  );
  process.exit(code);
}

const argv = process.argv.slice(2);
if (argv.includes("-h") || argv.includes("--help")) {
  printUsageAndExit(0);
}
if (argv.length === 0) {
  printUsageAndExit(2);
}

const name = argv[0];

try {
  scaffold({ name });
  process.stdout.write(nextSteps({ name }));
} catch (err) {
  if (err instanceof ScaffoldError) {
    process.stderr.write(`create-icp-commerce: ${err.message}\n`);
    process.exit(1);
  }
  throw err;
}
