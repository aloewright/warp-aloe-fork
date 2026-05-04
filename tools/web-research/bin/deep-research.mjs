#!/usr/bin/env node
// CLI entry for deep-research. Reads a goal, runs the iterative loop, and
// prints the final markdown report (or a JSON envelope with --json).

import { runDeepResearch } from "../src/deep-research.mjs";

function parseArgs(argv) {
  const opts = { json: false, quiet: false, goal: "" };
  const rest = [];
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--json") opts.json = true;
    else if (a === "--quiet") opts.quiet = true;
    else if (a === "--max-depth") opts.maxDepth = Number(argv[++i]);
    else if (a === "--max-seconds") opts.maxSeconds = Number(argv[++i]);
    else if (a === "--max-urls") opts.maxUrlsPerStep = Number(argv[++i]);
    else if (a === "-h" || a === "--help") opts.help = true;
    else rest.push(a);
  }
  opts.goal = rest.join(" ").trim();
  return opts;
}

async function readStdin() {
  if (process.stdin.isTTY) return "";
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  return Buffer.concat(chunks).toString("utf8").trim();
}

function usage() {
  return `Usage: deep-research [options] <goal>

Options:
  --json              Emit a JSON envelope on stdout (for programmatic use)
  --quiet             Suppress per-iteration progress on stderr
  --max-depth N       Iteration depth budget (default 7)
  --max-seconds N     Wall-clock budget in seconds (default 270)
  --max-urls N        Max URLs to extract per iteration (default 3)
  -h, --help          Show this help

Reads <goal> from argv or stdin. Requires FIRECRAWL_API_KEY plus one of
ANTHROPIC_API_KEY / OPENAI_API_KEY / OPENROUTER_API_KEY in the environment.

Without --json, the markdown research report is printed to stdout.`;
}

async function main() {
  const opts = parseArgs(process.argv);
  if (opts.help) {
    process.stdout.write(`${usage()}\n`);
    return;
  }
  if (!opts.goal) opts.goal = await readStdin();
  if (!opts.goal) {
    process.stderr.write(`${usage()}\n`);
    process.exit(2);
  }

  const log = opts.quiet
    ? () => {}
    : (event) => process.stderr.write(`[deep-research] ${JSON.stringify(event)}\n`);

  const result = await runDeepResearch({
    goal: opts.goal,
    maxDepth: opts.maxDepth,
    maxSeconds: opts.maxSeconds,
    maxUrlsPerStep: opts.maxUrlsPerStep,
    log,
  });

  if (opts.json) {
    process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
  } else {
    process.stdout.write(`${result.report}\n`);
  }
}

main().catch((err) => {
  process.stderr.write(`[deep-research] fatal: ${err.stack ?? err.message ?? err}\n`);
  process.exit(1);
});
