/**
 * Webhook signature validators (PDX-19 stubs).
 *
 * The cloud control plane exposes three webhook intake endpoints:
 *
 *   - `POST /api/webhooks/github`   — GitHub HMAC SHA-256 (`X-Hub-Signature-256`)
 *   - `POST /api/webhooks/slack`    — Slack v0 HMAC (`X-Slack-Signature` + `X-Slack-Request-Timestamp`)
 *   - `POST /api/webhooks/generic`  — generic HMAC SHA-256 (`X-Webhook-Signature`)
 *
 * Phase C ships only the *stubs*: HMAC validate, log, return 202. Routing
 * the validated payloads into the rest of the system is PDX-26 territory
 * (the in-repo `crates/github_webhook_receiver/` Rust receiver — for the
 * cloud port we'll wire up follow-on workflow invocations there).
 *
 * All three validators are constant-time: signatures are compared byte-by-byte
 * via `timingSafeEqual` so we don't leak validity timing. Validation
 * failures are logged but never echo signature bytes back into the response.
 */

const encoder = new TextEncoder();

/** Constant-time byte comparison. Returns false for unequal lengths. */
export function timingSafeEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) {
    diff |= (a[i] ?? 0) ^ (b[i] ?? 0);
  }
  return diff === 0;
}

function hexToBytes(hex: string): Uint8Array | null {
  if (hex.length % 2 !== 0) return null;
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i += 1) {
    const byte = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
    if (Number.isNaN(byte)) return null;
    out[i] = byte;
  }
  return out;
}

function bytesToHex(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i += 1) {
    s += (bytes[i] ?? 0).toString(16).padStart(2, "0");
  }
  return s;
}

async function hmacSha256(secret: string, payload: string): Promise<Uint8Array> {
  const key = await crypto.subtle.importKey(
    "raw",
    encoder.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"]
  );
  const sig = await crypto.subtle.sign("HMAC", key, encoder.encode(payload));
  return new Uint8Array(sig);
}

// ── GitHub ──────────────────────────────────────────────────────────────────

/**
 * Verify a GitHub webhook signature. GitHub sends `X-Hub-Signature-256` as
 * `sha256=<hex>` over the raw request body using the per-hook secret.
 * https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries
 */
export async function verifyGitHubSignature(
  secret: string,
  rawBody: string,
  signatureHeader: string | null
): Promise<boolean> {
  if (!signatureHeader || !signatureHeader.startsWith("sha256=")) return false;
  const expected = hexToBytes(signatureHeader.slice("sha256=".length));
  if (!expected) return false;
  const actual = await hmacSha256(secret, rawBody);
  return timingSafeEqual(actual, expected);
}

// ── Slack ───────────────────────────────────────────────────────────────────

/** Reject Slack requests older than this many seconds (defends against replay). */
export const SLACK_REPLAY_WINDOW_SECONDS = 60 * 5;

/**
 * Verify a Slack webhook signature. Slack signs
 * `v0:{timestamp}:{rawBody}` with the signing secret and sends the result as
 * `v0=<hex>` in `X-Slack-Signature`. The timestamp is in `X-Slack-Request-Timestamp`.
 * https://api.slack.com/authentication/verifying-requests-from-slack
 */
export async function verifySlackSignature(
  secret: string,
  rawBody: string,
  signatureHeader: string | null,
  timestampHeader: string | null,
  now: number = Math.floor(Date.now() / 1000)
): Promise<boolean> {
  if (!signatureHeader || !signatureHeader.startsWith("v0=")) return false;
  if (!timestampHeader) return false;
  const ts = Number.parseInt(timestampHeader, 10);
  if (!Number.isFinite(ts)) return false;
  if (Math.abs(now - ts) > SLACK_REPLAY_WINDOW_SECONDS) return false;
  const expected = hexToBytes(signatureHeader.slice("v0=".length));
  if (!expected) return false;
  const actual = await hmacSha256(secret, `v0:${ts}:${rawBody}`);
  return timingSafeEqual(actual, expected);
}

// ── Generic ─────────────────────────────────────────────────────────────────

/**
 * Verify a generic webhook signature. Takes the raw body and a hex-encoded
 * SHA-256 HMAC in `X-Webhook-Signature` (with optional `sha256=` prefix).
 * Per-source secrets can be plumbed in by the route handler — the validator
 * itself just compares.
 */
export async function verifyGenericSignature(
  secret: string,
  rawBody: string,
  signatureHeader: string | null
): Promise<boolean> {
  if (!signatureHeader) return false;
  const cleaned = signatureHeader.startsWith("sha256=")
    ? signatureHeader.slice("sha256=".length)
    : signatureHeader;
  const expected = hexToBytes(cleaned);
  if (!expected) return false;
  const actual = await hmacSha256(secret, rawBody);
  return timingSafeEqual(actual, expected);
}

/** Helper used by tests to pre-compute a valid signature. */
export async function signGitHubPayload(secret: string, body: string): Promise<string> {
  return `sha256=${bytesToHex(await hmacSha256(secret, body))}`;
}

/** Helper used by tests to pre-compute a valid Slack signature. */
export async function signSlackPayload(
  secret: string,
  timestamp: number,
  body: string
): Promise<string> {
  return `v0=${bytesToHex(await hmacSha256(secret, `v0:${timestamp}:${body}`))}`;
}

/** Helper used by tests to pre-compute a valid generic signature. */
export async function signGenericPayload(secret: string, body: string): Promise<string> {
  return bytesToHex(await hmacSha256(secret, body));
}
