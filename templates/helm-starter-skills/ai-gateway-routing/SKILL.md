---
name: ai-gateway-routing
description: Route every LLM, embedding, image, audio, and video model call through a Cloudflare AI Gateway dynamic route. Never call providers (OpenAI, Anthropic, Workers AI) directly from a Worker, Node script, or one-off.
roles: []
tags: [cloudflare, ai-gateway, llm, embeddings, secrets]
---

# ai-gateway-routing

All model traffic in this org goes through Cloudflare AI Gateway *dynamic routes*. Direct provider calls bypass caching, rate limits, observability, cost routing, fallbacks, and BYOK virtual keys, and they hardcode model choice — treat any direct call as a bug to fix.

This applies even when the existing code calls a provider directly. Route it through the gateway and pick the dynamic route by **capability**, not by provider model id.

## When to use

- You are about to import `openai`, `@anthropic-ai/sdk`, `replicate`, `fal`, or any provider SDK.
- You are reaching for `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` in env.
- You are constructing a URL like `https://api.openai.com/...` or `https://api.anthropic.com/...`.
- You see existing code that does any of the above — fix it.

Stop and route through the gateway instead.

## Endpoint

OpenAI-compatible universal endpoint:

```
https://gateway.ai.cloudflare.com/v1/${CF_ACCOUNT_ID}/${CF_GATEWAY_ID}/compat/chat/completions
```

Default `CF_GATEWAY_ID = "x"` in this account.

## Headers

```
Content-Type: application/json
cf-aig-authorization: Bearer ${CF_AIG_TOKEN}
cf-aig-zdr: true
```

Pull `CF_AIG_TOKEN` from Doppler — never inline it.

## Dynamic routes (pick by capability)

| Slug                   | Use for                              |
| ---------------------- | ------------------------------------ |
| `dynamic/text_gen`     | chat / text completion (default LLM) |
| `dynamic/research_gen` | deep-reasoning completions          |
| `dynamic/ai_embed`     | embeddings                           |
| `dynamic/image_gen`    | image generation                     |
| `dynamic/audio_gen`    | TTS / audio                          |
| `dynamic/stt_gen`      | speech-to-text                       |
| `dynamic/video_gen`    | video generation                     |

Pass the slug as the `model` field in the OpenAI-compatible body. Never pass a raw `openai/gpt-…` or `anthropic/claude-…` id — the route handles model selection inside Cloudflare.

## Inside a Worker (preferred)

Use the `AI` binding so the call is in-region and skips the public internet:

```ts
// wrangler.toml: [ai] binding = "AI"
const res = await env.AI.run(
  "dynamic/text_gen",
  { messages: [{ role: "user", content: "Summarise this PR." }] },
  { gateway: { id: "x" } },
);
```

For embeddings:

```ts
const { data } = await env.AI.run(
  "dynamic/ai_embed",
  { input: ["chunk one", "chunk two"] },
  { gateway: { id: "x" } },
);
```

## From a Node script (build, seed, migration, eval)

No SDK, no provider key. Plain `fetch` to the universal endpoint:

```ts
const res = await fetch(
  `https://gateway.ai.cloudflare.com/v1/${process.env.CF_ACCOUNT_ID}/x/compat/chat/completions`,
  {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "cf-aig-authorization": `Bearer ${process.env.CF_AIG_TOKEN}`,
      "cf-aig-zdr": "true",
    },
    body: JSON.stringify({
      model: "dynamic/text_gen",
      messages: [{ role: "user", content: "Hello" }],
    }),
  },
);
const json = await res.json();
```

Run the script under Doppler so the token is present:

```bash
doppler run --scope . -- node scripts/seed.ts
```

## Canonical reference implementation

Read and reuse — don't reinvent:

- `cloudos/shared/gateway/src/routes.ts` — `chatCompletion`, `researchCompletion`
- `cloudos/shared/gateway/src/gateway.ts` — request shaping, retry, ZDR
- `cloudos/shared/gateway/src/llm.ts` — `gatewayEmbedding`

## Anti-patterns (block these in review)

- `import OpenAI from "openai"` in any file in this monorepo.
- `import Anthropic from "@anthropic-ai/sdk"` in any file in this monorepo.
- `process.env.OPENAI_API_KEY` or `process.env.ANTHROPIC_API_KEY` reads.
- Hardcoded `model: "gpt-4o"` / `model: "claude-3-5-sonnet"` strings.
- A `fetch` to `api.openai.com` or `api.anthropic.com`.

If you see one of these, flag it and replace with a `dynamic/*` call.
