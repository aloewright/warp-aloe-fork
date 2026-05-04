# web-research

Two web-research CLIs that Warp agents (and humans) can spawn from chat or
hooks.

- `web-agent` — a port of [firecrawl/web-agent](https://github.com/firecrawl/web-agent)'s
  loop: an LLM drives Firecrawl's `search`, `scrape`, and `extract` tools to
  satisfy a goal, then emits a markdown answer.
- `deep-research` — a port of the iterative loop from
  [aloewright/open-deep-research](https://github.com/aloewright/open-deep-research):
  search → extract → reasoning-model gap analysis → repeat → final long-form
  report.

Both are pure ESM Node (no build, no native deps) and talk to Firecrawl's REST
API plus one of Anthropic / OpenAI / OpenRouter for reasoning.

## Setup

```bash
cp tools/web-research/.env.example tools/web-research/.env
# fill in FIRECRAWL_API_KEY plus at least one of:
#   ANTHROPIC_API_KEY, OPENAI_API_KEY, OPENROUTER_API_KEY
```

The CLIs only read from `process.env`, so source the file (or use a tool like
`direnv` / `dotenvx`) before invoking:

```bash
set -a; source tools/web-research/.env; set +a
```

No `npm install` is required — there are no third-party runtime dependencies.

## Usage

```bash
# Quick web research with the agent (search/scrape/extract loop).
node tools/web-research/bin/web-agent.mjs "Find the current price of WGSL spec changes since v1"

# Iterative deep research with a long-form markdown report.
node tools/web-research/bin/deep-research.mjs "Compare Wayland and X11 input pipelines for terminal emulators"

# JSON envelope for programmatic callers (hooks, sub-agents, scripts).
node tools/web-research/bin/web-agent.mjs --json "find ratatui's MSRV"
node tools/web-research/bin/deep-research.mjs --json --max-depth 5 "summarize the state of GPU text rendering in Rust in 2026"
```

## Invoking from Claude Code

Two skills wrap these CLIs:

- `/web-agent` — quick targeted lookup
- `/deep-research` — multi-iteration research with synthesis

Sub-agents can also call the CLIs directly via Bash; the skills explain when
to pick each. See `.agents/skills/web-agent/SKILL.md` and
`.agents/skills/deep-research/SKILL.md`.

## Provider selection

Provider is auto-detected by which key is present, in order:

1. `ANTHROPIC_API_KEY` → Claude (default `claude-sonnet-4-6`)
2. `OPENAI_API_KEY` → GPT (default `gpt-4o`, reasoning default `o4-mini`)
3. `OPENROUTER_API_KEY` → routed (default `openai/gpt-4o`)

Override per-call with the `*_MODEL` / `REASONING_MODEL` env vars.

## Environment knobs

| Var                          | Purpose                                          | Default      |
| ---------------------------- | ------------------------------------------------ | ------------ |
| `FIRECRAWL_API_KEY`          | Firecrawl auth                                   | (required)   |
| `ANTHROPIC_API_KEY`          | Anthropic auth                                   | —            |
| `OPENAI_API_KEY`             | OpenAI auth                                      | —            |
| `OPENROUTER_API_KEY`         | OpenRouter auth                                  | —            |
| `ANTHROPIC_MODEL`            | Anthropic chat model                             | `claude-sonnet-4-6` |
| `OPENAI_MODEL`               | OpenAI chat model                                | `gpt-4o`     |
| `REASONING_MODEL`            | OpenAI reasoning model (o-series)                | `o4-mini`    |
| `OPENROUTER_MODEL`           | OpenRouter model                                 | `openai/gpt-4o` |
| `DEEP_RESEARCH_MAX_SECONDS`  | Wall-clock budget for `deep-research`            | `270`        |
| `DEEP_RESEARCH_MAX_DEPTH`    | Iteration budget for `deep-research`             | `7`          |
| `DEEP_RESEARCH_MAX_URLS`     | URLs extracted per iteration                     | `3`          |
