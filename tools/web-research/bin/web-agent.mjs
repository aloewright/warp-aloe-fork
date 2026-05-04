#!/usr/bin/env node
// CLI entry for the web-agent. Reads a goal from argv or stdin, runs the
// agent, and prints either a JSON envelope (--json) or a markdown answer.

import { runWebAgent } from "../src/web-agent.mjs";

function parseArgs(argv) {
  const opts = { json: false, maxSteps: 8, quiet: false, goal: "" };
  const rest = [];
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--json") opts.json = true;
    else if (a === "--quiet") opts.quiet = true;
    else if (a === "--max-steps") opts.maxSteps = Number(argv[++i]);
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
  return `Usage: web-agent [options] <goal>

Options:
  --json           Emit a JSON envelope on stdout (for programmatic use)
  --quiet          Suppress per-step progress on stderr
  --max-steps N    Maximum tool-use iterations (default 8)
  -h, --help       Show this help

Reads <goal> from argv or stdin. Requires FIRECRAWL_API_KEY plus one of
ANTHROPIC_API_KEY / OPENAI_API_KEY / OPENROUTER_API_KEY in the environment.`;
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
    : (event) => process.stderr.write(`[web-agent] ${JSON.stringify(event)}\n`);

  const result = await runWebAgent({ goal: opts.goal, maxSteps: opts.maxSteps, log });

  if (opts.json) {
    process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
  } else if (result.ok) {
    process.stdout.write(`${result.answer}\n`);
  } else {
    process.stderr.write(`[web-agent] ${result.error}\n`);
    process.exit(1);
  }
}

main().catch((err) => {
  process.stderr.write(`[web-agent] fatal: ${err.stack ?? err.message ?? err}\n`);
  process.exit(1);
});
