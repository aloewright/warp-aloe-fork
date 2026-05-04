---
name: web-agent
description: Run a Firecrawl-powered web agent that searches, scrapes, and extracts to answer a focused web research question. Use when the user wants up-to-date information from the web (release notes, library MSRVs, current state of a spec, comparing tools, finding docs), or when an implementation needs facts that aren't in the repo. Prefer this over plain WebSearch when the answer requires combining multiple pages or extracting structured data.
---

# web-agent

A thin wrapper around `tools/web-research/bin/web-agent.mjs` — a port of
[firecrawl/web-agent](https://github.com/firecrawl/web-agent). The agent picks
between `search`, `scrape`, and `extract` tools in a loop until it has enough
to answer the goal.

## When to use

- Up-to-date facts: "What's the current MSRV of `wgpu`?"
- Multi-page lookups: "Find the upstream issue tracking X across crates.io and GitHub"
- Structured extraction: "List the pricing tiers from these three SaaS pages"
- Anything where a single web search is not enough but full deep research is overkill

For exploratory, multi-angle research with synthesis (e.g. "compare Wayland
vs X11 input pipelines for terminal emulators"), use the `deep-research`
skill instead.

## Prerequisites

Environment variables (loaded from `tools/web-research/.env` in normal use):

- `FIRECRAWL_API_KEY` (required)
- One of `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`

If the keys are missing, tell the user which ones to set and stop — do not
guess answers.

## Invocation

```bash
node tools/web-research/bin/web-agent.mjs "<goal>"
```

For programmatic use (parsing the result in another step), pass `--json`:

```bash
node tools/web-research/bin/web-agent.mjs --json "<goal>"
```

The JSON envelope shape:

```json
{
  "ok": true,
  "answer": "markdown answer with [n](url) citations",
  "steps": 3,
  "trace": [{ "step": 1, "decision": { ... }, "observation": { ... } }, ...]
}
```

## Tips for crafting goals

- One sentence. Specific. Include the year if the answer is time-sensitive.
- Say what shape the answer should take: "list the top 3 …", "give the version number".
- Cite the source domains you trust if known: "from crates.io and the wgpu changelog".

## Failure modes

- **Step budget exhausted.** Increase with `--max-steps 12` or narrow the goal.
- **Firecrawl rate-limit / 4xx.** Surface the error to the user; do not silently retry.
- **LLM returned non-JSON.** The agent self-corrects once; if the loop produces
  no `finish` action, treat the partial trace as the answer and explain the limit.

## Hook entrypoint

The `.claude/settings.json` config exposes this skill via Bash so sub-agents
can invoke it without a slash command. Use the explicit form above when calling
from your own code or another skill.
