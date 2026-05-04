// Port of the open-deep-research iterative loop. Faithful to the algorithm
// described in aloewright/open-deep-research's chat route:
//
//   1. Init: time budget, depth budget, accumulators for findings/summaries.
//   2. Loop up to maxDepth times:
//        a. Search the current topic with Firecrawl.
//        b. Extract structured info from the top URLs.
//        c. Ask the reasoning model to synthesize and propose gaps.
//        d. If shouldContinue=false or budget exhausted, break.
//        e. Otherwise pick the next gap as the new topic.
//   3. Final synthesis: long-form analysis from accumulated findings.
//
// Returns { ok, report, depth, findings, summaries, sources, trace }.

import { FirecrawlClient } from "./firecrawl.mjs";
import { LlmClient, parseJsonLoose } from "./llm.mjs";

const DEFAULT_MAX_DEPTH = Number(process.env.DEEP_RESEARCH_MAX_DEPTH ?? 7);
const DEFAULT_MAX_SECONDS = Number(process.env.DEEP_RESEARCH_MAX_SECONDS ?? 270);
const DEFAULT_MAX_URLS = Number(process.env.DEEP_RESEARCH_MAX_URLS ?? 3);

const ANALYSIS_SYSTEM = `You are a research analyst. Given a topic, the user's
overall goal, and a list of findings collected so far, you decide what to do
next. Reply with a single JSON object in a fenced \`\`\`json block:

{
  "summary": "2-5 sentence synthesis of what we now know about the topic",
  "gaps": ["short phrase describing remaining open question", ...],
  "shouldContinue": true | false,
  "nextSearchTopic": "next focused query to run" | null,
  "urlToSearch": "specific URL worth extracting next" | null
}

Be honest about gaps. Prefer to stop ("shouldContinue": false) once the
overall goal can be answered well from the findings.`;

const FINAL_SYSTEM = `You are a research analyst writing a comprehensive final
report. Use the findings and summaries to produce a thorough, well-organized
markdown report that fully answers the user's goal. Cite sources inline as
[n](url) where n matches the source list provided. Include a short executive
summary at the top and a "Sources" section at the bottom.`;

function clip(text, max) {
  if (!text) return "";
  return text.length > max ? `${text.slice(0, max)}\n…[truncated]` : text;
}

function flattenExtract(result) {
  const data = result?.data ?? result;
  if (data == null) return [];
  if (Array.isArray(data)) return data.flatMap(flattenExtract);
  if (typeof data === "string") return [data];
  if (typeof data === "object") return [data];
  return [String(data)];
}

function findingsToText(findings, maxChars = 12000) {
  const parts = findings.map((f, i) => {
    const url = f.url ? ` (source: ${f.url})` : "";
    let body;
    if (typeof f.value === "string") body = f.value;
    else {
      try {
        body = JSON.stringify(f.value, null, 2);
      } catch {
        body = String(f.value);
      }
    }
    return `[${i + 1}]${url}\n${body}`;
  });
  let total = 0;
  const kept = [];
  for (const p of parts) {
    total += p.length;
    if (total > maxChars) {
      kept.push("…[older findings truncated]");
      break;
    }
    kept.push(p);
  }
  return kept.join("\n\n");
}

export async function runDeepResearch({
  goal,
  maxDepth = DEFAULT_MAX_DEPTH,
  maxSeconds = DEFAULT_MAX_SECONDS,
  maxUrlsPerStep = DEFAULT_MAX_URLS,
  log = () => {},
  firecrawl,
  llm,
} = {}) {
  if (!goal) throw new Error("goal is required");
  const fc = firecrawl ?? new FirecrawlClient({ apiKey: process.env.FIRECRAWL_API_KEY });
  const ai = llm ?? new LlmClient();

  const deadline = Date.now() + maxSeconds * 1000;
  const findings = [];
  const summaries = [];
  const sources = new Map(); // url -> { url, title }
  const trace = [];
  const seenQueries = new Set();
  let topic = goal;
  let depth = 0;
  let consecutiveFailures = 0;

  while (depth < maxDepth) {
    if (Date.now() > deadline) {
      log({ event: "deadline.reached", depth });
      break;
    }
    depth++;
    log({ event: "iter.begin", depth, topic });
    const iter = { depth, topic, errors: [] };

    // 1. Search
    let searchResult;
    try {
      const queryKey = topic.toLowerCase().trim();
      if (seenQueries.has(queryKey)) {
        log({ event: "search.skip_duplicate", topic });
      } else {
        seenQueries.add(queryKey);
        searchResult = await fc.search(topic, { limit: 5 });
        log({ event: "search.done", topic, count: (searchResult?.data ?? []).length });
      }
    } catch (err) {
      iter.errors.push(`search: ${err.message}`);
      consecutiveFailures++;
      log({ event: "search.error", message: err.message });
    }

    const items = searchResult?.data ?? searchResult?.web ?? [];
    for (const it of items) {
      const url = it.url ?? it.link;
      if (url && !sources.has(url)) sources.set(url, { url, title: it.title ?? "" });
    }
    iter.searchUrls = items.map((it) => it.url ?? it.link).filter(Boolean);

    // 2. Extract from top N URLs
    const targetUrls = iter.searchUrls.slice(0, maxUrlsPerStep);
    if (targetUrls.length > 0) {
      try {
        const extractResult = await fc.extract(targetUrls, {
          prompt: `Extract the key information needed to answer: ${goal}\n\nFocus area for this iteration: ${topic}`,
        });
        const items = flattenExtract(extractResult);
        for (const value of items) {
          findings.push({ url: targetUrls.join(","), value });
        }
        log({ event: "extract.done", urls: targetUrls.length, items: items.length });
        consecutiveFailures = 0;
      } catch (err) {
        iter.errors.push(`extract: ${err.message}`);
        consecutiveFailures++;
        log({ event: "extract.error", message: err.message });
      }
    }

    // 3. Analyze with reasoning model
    let analysis = null;
    try {
      const completion = await ai.complete({
        system: ANALYSIS_SYSTEM,
        messages: [
          {
            role: "user",
            content:
              `Overall goal: ${goal}\n\n` +
              `Current focus topic: ${topic}\n\n` +
              `Findings so far:\n${findingsToText(findings)}\n\n` +
              `Decide whether to continue and, if so, the next focused topic.`,
          },
        ],
        maxTokens: 2048,
        temperature: 0.2,
        reasoning: true,
      });
      analysis = parseJsonLoose(completion.text);
      if (analysis?.summary) summaries.push({ depth, topic, summary: analysis.summary });
      log({ event: "analysis.done", depth, shouldContinue: analysis?.shouldContinue });
    } catch (err) {
      iter.errors.push(`analysis: ${err.message}`);
      consecutiveFailures++;
      log({ event: "analysis.error", message: err.message });
    }

    iter.analysis = analysis;
    trace.push(iter);

    if (consecutiveFailures >= 3) {
      log({ event: "abort.too_many_failures", depth });
      break;
    }
    if (!analysis || analysis.shouldContinue === false) break;

    // 4. Pick next topic from gaps or model suggestion
    const nextTopic = analysis.nextSearchTopic || (analysis.gaps && analysis.gaps[0]);
    if (!nextTopic) break;
    topic = nextTopic;
  }

  // 5. Final synthesis
  log({ event: "synthesis.begin" });
  const sourceList = [...sources.values()];
  let report = "";
  try {
    const sourcesBlock = sourceList
      .map((s, i) => `[${i + 1}] ${s.title || "(untitled)"} — ${s.url}`)
      .join("\n");
    const summariesBlock = summaries
      .map((s, i) => `(${i + 1}) [${s.topic}] ${s.summary}`)
      .join("\n\n");
    const completion = await ai.complete({
      system: FINAL_SYSTEM,
      messages: [
        {
          role: "user",
          content:
            `Goal: ${goal}\n\n` +
            `Sources (use these numbers for citations):\n${sourcesBlock}\n\n` +
            `Iteration summaries:\n${summariesBlock}\n\n` +
            `Findings:\n${findingsToText(findings, 16000)}\n\n` +
            `Write the final markdown report now.`,
        },
      ],
      maxTokens: 8192,
      temperature: 0.3,
      reasoning: true,
    });
    report = completion.text;
  } catch (err) {
    log({ event: "synthesis.error", message: err.message });
    report = `# Research report (synthesis failed)\n\n${err.message}\n\n` + clip(findingsToText(findings, 8000), 8000);
  }

  return {
    ok: true,
    report,
    depth,
    findings,
    summaries,
    sources: sourceList,
    trace,
  };
}
