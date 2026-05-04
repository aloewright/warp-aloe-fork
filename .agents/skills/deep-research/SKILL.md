---
name: deep-research
description: Run an iterative deep-research loop (port of aloewright/open-deep-research) that searches the web, extracts evidence, and uses a reasoning model to synthesize a comprehensive markdown report. Use when the user asks for "deep research", "a research report", "compare X vs Y across the industry", or any open-ended investigation that needs multiple search-extract-analyze rounds rather than a single lookup. Slower than the `web-agent` skill but produces a structured long-form report.
---

# deep-research

A thin wrapper around `tools/web-research/bin/deep-research.mjs` — a port of
the iterative loop from
[aloewright/open-deep-research](https://github.com/aloewright/open-deep-research).
On each iteration:

1. Search the current focus topic with Firecrawl.
2. Extract structured info from the top URLs.
3. Ask a reasoning model to synthesize and propose remaining gaps.
4. Move to the next gap, or stop and emit a long-form markdown report.

## When to use

- Open-ended research questions: "What's the state of GPU text rendering in Rust in 2026?"
- Comparative reports: "Compare leading open-source web-agent frameworks."
- Background briefings: "Brief me on the WGSL spec changes shipping this year."

For a single targeted lookup ("what's the MSRV of wgpu?") use the `web-agent`
skill — deep-research is heavier and burns API calls + minutes.

## Prerequisites

Environment variables (loaded from `tools/web-research/.env` in normal use):

- `FIRECRAWL_API_KEY` (required)
- One of `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`

If the keys are missing, tell the user which ones to set and stop — do not
guess answers.

## Invocation

```bash
node tools/web-research/bin/deep-research.mjs "<goal>"
```

Knobs (all optional):

```bash
node tools/web-research/bin/deep-research.mjs \
  --max-depth 5 \
  --max-seconds 180 \
  --max-urls 4 \
  --json \
  "<goal>"
```

JSON envelope shape:

```json
{
  "ok": true,
  "report": "markdown report with [n](url) citations",
  "depth": 5,
  "findings": [...],
  "summaries": [...],
  "sources": [{ "url": "...", "title": "..." }, ...],
  "trace": [{ "depth": 1, "topic": "...", "analysis": { ... } }, ...]
}
```

## Tips for crafting goals

- State the angle: "for terminal emulators", "from a Rust + macOS perspective", "in 2026".
- Bound the scope: "compare the top 3 …", "summarize the last 12 months of …".
- Say what the deliverable should cover ("trade-offs", "decision criteria", "code examples").

## Cost & latency

Each iteration costs one Firecrawl `search`, up to N `extract` calls, and one
LLM analysis call, plus a final synthesis. With defaults (depth 7, 3 URLs)
expect 1-4 minutes and tens of API calls. Drop `--max-depth` to 3 for a
cheaper "lite" run.

## Failure modes

- **Wall-clock deadline reached.** The loop stops cleanly and synthesizes from
  whatever it has. Report depth and source count to the user.
- **3 consecutive tool failures.** The loop aborts; surface the last error.
- **Synthesis failed.** Findings are dumped instead of a polished report.

## Hook entrypoint

The `.claude/settings.json` config exposes this skill via Bash so sub-agents
can invoke it without a slash command. Use the explicit form above when calling
from your own code or another skill.
