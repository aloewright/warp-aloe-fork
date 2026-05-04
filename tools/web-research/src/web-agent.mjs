// Lightweight port of firecrawl/web-agent's core loop. Given a goal, the
// agent picks a tool (search, scrape, extract, or finish), runs it, observes
// the result, and iterates until it decides the goal is met or the step
// budget is exhausted. The reasoning happens in the LLM; this module is just
// the orchestrator and tool-runner.

import { FirecrawlClient } from "./firecrawl.mjs";
import { LlmClient, parseJsonLoose } from "./llm.mjs";

const SYSTEM_PROMPT = `You are a web research agent. You have these tools:

- search(query, limit=5): web search, returns list of {url, title, description}
- scrape(url): fetch a single page as markdown
- extract(urls, prompt): use an LLM-backed extractor over one or more URLs to pull structured info
- finish(answer): emit your final answer and stop

On each turn, respond with a SINGLE JSON object in a fenced \`\`\`json block:

{
  "thought": "one or two sentences on what you've learned and what to do next",
  "action": "search" | "scrape" | "extract" | "finish",
  "args": { ... }   // shape depends on action
}

Argument shapes:
- search: { "query": string, "limit"?: number }
- scrape: { "url": string }
- extract: { "urls": string[], "prompt": string }
- finish: { "answer": string }   // markdown, may include citations as [n](url)

Be terse. Do not call the same query twice. Prefer extract() over scrape() when
you know what fields you need. Stop as soon as you have enough to answer.`;

function clip(text, max = 1200) {
  if (!text) return "";
  return text.length > max ? `${text.slice(0, max)}\n…[truncated ${text.length - max} chars]` : text;
}

function summarizeSearch(result) {
  const items = result?.data ?? result?.web ?? [];
  return items
    .slice(0, 10)
    .map((it, i) => {
      const url = it.url ?? it.link ?? "";
      const title = it.title ?? "";
      const desc = it.description ?? it.snippet ?? "";
      return `${i + 1}. ${title}\n   ${url}\n   ${clip(desc, 240)}`;
    })
    .join("\n");
}

function summarizeScrape(result) {
  const md = result?.data?.markdown ?? result?.markdown ?? "";
  const meta = result?.data?.metadata ?? {};
  const head = meta.title ? `# ${meta.title}\n` : "";
  return `${head}${clip(md, 4000)}`;
}

function summarizeExtract(result) {
  const data = result?.data ?? result;
  try {
    return clip(JSON.stringify(data, null, 2), 4000);
  } catch {
    return clip(String(data), 4000);
  }
}

export async function runWebAgent({
  goal,
  maxSteps = 8,
  log = () => {},
  firecrawl,
  llm,
} = {}) {
  if (!goal) throw new Error("goal is required");
  const fc = firecrawl ?? new FirecrawlClient({ apiKey: process.env.FIRECRAWL_API_KEY });
  const ai = llm ?? new LlmClient();

  const history = [{ role: "user", content: `Goal: ${goal}` }];
  const trace = [];

  for (let step = 1; step <= maxSteps; step++) {
    log({ event: "step.begin", step });
    const completion = await ai.complete({
      system: SYSTEM_PROMPT,
      messages: history,
      maxTokens: 2048,
      temperature: 0.2,
    });
    const decision = parseJsonLoose(completion.text);
    if (!decision || !decision.action) {
      log({ event: "agent.parse_error", text: completion.text });
      history.push({ role: "assistant", content: completion.text });
      history.push({
        role: "user",
        content: 'Your previous response was not valid JSON. Reply with a single fenced ```json block matching the schema.',
      });
      continue;
    }
    log({ event: "agent.decision", step, decision });
    history.push({ role: "assistant", content: completion.text });
    trace.push({ step, decision });

    let observation;
    try {
      observation = await runAction(decision, fc);
    } catch (err) {
      observation = { ok: false, error: err.message };
    }
    log({ event: "agent.observation", step, action: decision.action, ok: observation.ok });
    trace[trace.length - 1].observation = observation;

    if (decision.action === "finish") {
      return {
        ok: true,
        answer: decision.args?.answer ?? "",
        steps: step,
        trace,
      };
    }
    history.push({
      role: "user",
      content: `Observation:\n${observation.summary ?? observation.error ?? "(empty)"}`,
    });
  }

  return {
    ok: false,
    answer: "",
    steps: maxSteps,
    trace,
    error: `Step budget (${maxSteps}) exhausted without finish.`,
  };
}

async function runAction(decision, fc) {
  const args = decision.args ?? {};
  switch (decision.action) {
    case "search": {
      if (!args.query) return { ok: false, error: "search requires args.query" };
      const result = await fc.search(args.query, { limit: args.limit ?? 5 });
      return { ok: true, summary: summarizeSearch(result), raw: result };
    }
    case "scrape": {
      if (!args.url) return { ok: false, error: "scrape requires args.url" };
      const result = await fc.scrape(args.url);
      return { ok: true, summary: summarizeScrape(result), raw: result };
    }
    case "extract": {
      if (!args.urls || args.urls.length === 0)
        return { ok: false, error: "extract requires args.urls (array)" };
      if (!args.prompt) return { ok: false, error: "extract requires args.prompt" };
      const result = await fc.extract(args.urls, { prompt: args.prompt });
      return { ok: true, summary: summarizeExtract(result), raw: result };
    }
    case "finish":
      return { ok: true, summary: "(finished)" };
    default:
      return { ok: false, error: `Unknown action: ${decision.action}` };
  }
}
