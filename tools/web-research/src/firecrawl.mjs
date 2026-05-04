// Minimal Firecrawl REST client. Documented at https://docs.firecrawl.dev
//
// We hit the v2 endpoints and fall back to v1 on 404 so the client keeps
// working while Firecrawl rolls APIs forward. Only the surface used by the
// web-agent and deep-research loop is implemented.

const DEFAULT_BASE = "https://api.firecrawl.dev";

export class FirecrawlError extends Error {
  constructor(message, { status, body } = {}) {
    super(message);
    this.name = "FirecrawlError";
    this.status = status;
    this.body = body;
  }
}

export class FirecrawlClient {
  constructor({ apiKey, baseUrl = DEFAULT_BASE, fetchImpl = globalThis.fetch } = {}) {
    if (!apiKey) throw new FirecrawlError("FIRECRAWL_API_KEY is required");
    this.apiKey = apiKey;
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.fetch = fetchImpl;
  }

  async #post(path, body) {
    const res = await this.fetch(`${this.baseUrl}${path}`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${this.apiKey}`,
      },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    let data;
    try {
      data = text ? JSON.parse(text) : {};
    } catch {
      data = { raw: text };
    }
    if (!res.ok) {
      throw new FirecrawlError(
        `Firecrawl ${path} ${res.status}: ${data?.error ?? data?.message ?? text.slice(0, 200)}`,
        { status: res.status, body: data },
      );
    }
    return data;
  }

  async #postWithFallback(v2Path, v1Path, body) {
    try {
      return await this.#post(v2Path, body);
    } catch (err) {
      if (err.status === 404 || err.status === 405) {
        return await this.#post(v1Path, body);
      }
      throw err;
    }
  }

  // Search the web. Returns { data: [{ url, title, description, ... }, ...] }.
  async search(query, { limit = 5, lang, country, scrapeOptions } = {}) {
    const body = { query, limit };
    if (lang) body.lang = lang;
    if (country) body.country = country;
    if (scrapeOptions) body.scrapeOptions = scrapeOptions;
    return this.#postWithFallback("/v2/search", "/v1/search", body);
  }

  // Scrape a single URL. Returns { data: { markdown, html, metadata, ... } }.
  async scrape(url, { formats = ["markdown"], onlyMainContent = true } = {}) {
    const body = { url, formats, onlyMainContent };
    return this.#postWithFallback("/v2/scrape", "/v1/scrape", body);
  }

  // Extract structured data from one or more URLs given a natural-language
  // prompt. Returns { data: ... } with the LLM's structured output.
  async extract(urls, { prompt, schema, enableWebSearch = false } = {}) {
    const body = { urls: Array.isArray(urls) ? urls : [urls] };
    if (prompt) body.prompt = prompt;
    if (schema) body.schema = schema;
    if (enableWebSearch) body.enableWebSearch = true;
    return this.#postWithFallback("/v2/extract", "/v1/extract", body);
  }
}
