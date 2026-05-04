// Tiny multi-provider LLM client. Supports Anthropic, OpenAI, and OpenRouter.
//
// Auto-selects a provider based on which API key is present. Caller can also
// pass `provider` explicitly. Returns plain strings; for JSON-shaped output,
// callers prompt for JSON and parse with parseJsonLoose() below.

export class LlmError extends Error {
  constructor(message, { status, body, provider } = {}) {
    super(message);
    this.name = "LlmError";
    this.status = status;
    this.body = body;
    this.provider = provider;
  }
}

function pickProvider(env) {
  if (env.ANTHROPIC_API_KEY) return "anthropic";
  if (env.OPENAI_API_KEY) return "openai";
  if (env.OPENROUTER_API_KEY) return "openrouter";
  return null;
}

export function detectProvider(env = process.env) {
  const provider = pickProvider(env);
  if (!provider) {
    throw new LlmError(
      "No LLM provider configured. Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or OPENROUTER_API_KEY.",
    );
  }
  return provider;
}

export class LlmClient {
  constructor({ provider, env = process.env, fetchImpl = globalThis.fetch } = {}) {
    this.env = env;
    this.fetch = fetchImpl;
    this.provider = provider ?? detectProvider(env);
  }

  // Generate a single completion. `system` and `messages` use the standard
  // chat shape ({role: "user"|"assistant", content: string}).
  // Use `{ reasoning: true }` to prefer a reasoning model when one is
  // configured (only meaningful for OpenAI / OpenRouter).
  async complete({ system, messages, model, maxTokens = 4096, temperature = 0.2, reasoning = false }) {
    if (this.provider === "anthropic") {
      return this.#anthropic({ system, messages, model, maxTokens, temperature });
    }
    if (this.provider === "openai") {
      return this.#openai({ system, messages, model, maxTokens, temperature, reasoning });
    }
    if (this.provider === "openrouter") {
      return this.#openrouter({ system, messages, model, maxTokens, temperature, reasoning });
    }
    throw new LlmError(`Unknown provider: ${this.provider}`);
  }

  async #anthropic({ system, messages, model, maxTokens, temperature }) {
    const chosen = model ?? this.env.ANTHROPIC_MODEL ?? "claude-sonnet-4-6";
    const res = await this.fetch("https://api.anthropic.com/v1/messages", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "x-api-key": this.env.ANTHROPIC_API_KEY,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: chosen,
        max_tokens: maxTokens,
        temperature,
        system,
        messages,
      }),
    });
    return this.#parse(res, "anthropic", (data) => {
      const text = (data.content ?? [])
        .filter((c) => c.type === "text")
        .map((c) => c.text)
        .join("");
      return { text, model: chosen, raw: data };
    });
  }

  async #openai({ system, messages, model, maxTokens, temperature, reasoning }) {
    const chosen =
      model ??
      (reasoning ? this.env.REASONING_MODEL ?? "o4-mini" : this.env.OPENAI_MODEL ?? "gpt-4o");
    const fullMessages = system ? [{ role: "system", content: system }, ...messages] : messages;
    // Reasoning models reject `temperature` and use `max_completion_tokens`.
    const isReasoning = /^o\d/.test(chosen);
    const body = isReasoning
      ? { model: chosen, messages: fullMessages, max_completion_tokens: maxTokens }
      : { model: chosen, messages: fullMessages, max_tokens: maxTokens, temperature };
    const res = await this.fetch("https://api.openai.com/v1/chat/completions", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${this.env.OPENAI_API_KEY}`,
      },
      body: JSON.stringify(body),
    });
    return this.#parse(res, "openai", (data) => ({
      text: data.choices?.[0]?.message?.content ?? "",
      model: chosen,
      raw: data,
    }));
  }

  async #openrouter({ system, messages, model, maxTokens, temperature, reasoning }) {
    const chosen = model ?? this.env.OPENROUTER_MODEL ?? "openai/gpt-4o";
    const fullMessages = system ? [{ role: "system", content: system }, ...messages] : messages;
    const res = await this.fetch("https://openrouter.ai/api/v1/chat/completions", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${this.env.OPENROUTER_API_KEY}`,
        "HTTP-Referer": "https://warp.dev",
        "X-Title": "Warp web-research",
      },
      body: JSON.stringify({
        model: chosen,
        messages: fullMessages,
        max_tokens: maxTokens,
        temperature: reasoning ? undefined : temperature,
      }),
    });
    return this.#parse(res, "openrouter", (data) => ({
      text: data.choices?.[0]?.message?.content ?? "",
      model: chosen,
      raw: data,
    }));
  }

  async #parse(res, provider, extract) {
    const text = await res.text();
    let data;
    try {
      data = text ? JSON.parse(text) : {};
    } catch {
      data = { raw: text };
    }
    if (!res.ok) {
      throw new LlmError(
        `${provider} ${res.status}: ${data?.error?.message ?? data?.error ?? text.slice(0, 300)}`,
        { status: res.status, body: data, provider },
      );
    }
    return extract(data);
  }
}

// Pull a JSON object out of model output that may include prose, code fences,
// or trailing commentary. Returns null if no JSON object is found.
export function parseJsonLoose(text) {
  if (!text) return null;
  const fence = text.match(/```(?:json)?\s*([\s\S]*?)```/);
  const candidates = [];
  if (fence) candidates.push(fence[1]);
  candidates.push(text);
  for (const candidate of candidates) {
    const trimmed = candidate.trim();
    try {
      return JSON.parse(trimmed);
    } catch {
      // fall through
    }
    const start = trimmed.indexOf("{");
    const end = trimmed.lastIndexOf("}");
    if (start !== -1 && end !== -1 && end > start) {
      try {
        return JSON.parse(trimmed.slice(start, end + 1));
      } catch {
        // fall through
      }
    }
  }
  return null;
}
