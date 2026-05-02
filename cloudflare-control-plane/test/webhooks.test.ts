/**
 * Unit tests for the webhook HMAC validators (PDX-19).
 *
 * Round-trips the test-side signers against the validators so a regression
 * in either half of the pair surfaces immediately.
 */

import { describe, expect, it } from "vitest";

import {
  signGenericPayload,
  signGitHubPayload,
  signSlackPayload,
  verifyGenericSignature,
  verifyGitHubSignature,
  verifySlackSignature,
  timingSafeEqual
} from "../src/shared/webhooks.js";

describe("timingSafeEqual", () => {
  it("returns true for equal byte sequences", () => {
    expect(timingSafeEqual(new Uint8Array([1, 2, 3]), new Uint8Array([1, 2, 3]))).toBe(true);
  });
  it("returns false for unequal lengths", () => {
    expect(timingSafeEqual(new Uint8Array([1, 2]), new Uint8Array([1, 2, 3]))).toBe(false);
  });
  it("returns false for different bytes of equal length", () => {
    expect(timingSafeEqual(new Uint8Array([1, 2, 3]), new Uint8Array([1, 2, 4]))).toBe(false);
  });
});

describe("verifyGitHubSignature", () => {
  const secret = "github-secret";
  const body = '{"action":"opened"}';

  it("accepts a valid signature", async () => {
    const sig = await signGitHubPayload(secret, body);
    expect(await verifyGitHubSignature(secret, body, sig)).toBe(true);
  });

  it("rejects a tampered body", async () => {
    const sig = await signGitHubPayload(secret, body);
    expect(await verifyGitHubSignature(secret, body + "x", sig)).toBe(false);
  });

  it("rejects when the prefix is missing", async () => {
    const sig = await signGitHubPayload(secret, body);
    const bare = sig.replace("sha256=", "");
    expect(await verifyGitHubSignature(secret, body, bare)).toBe(false);
  });

  it("rejects null signature header", async () => {
    expect(await verifyGitHubSignature(secret, body, null)).toBe(false);
  });

  it("rejects malformed hex", async () => {
    expect(await verifyGitHubSignature(secret, body, "sha256=zz")).toBe(false);
  });
});

describe("verifySlackSignature", () => {
  const secret = "slack-secret";
  const body = "token=foo&team_id=T1";
  const ts = 1_700_000_000;

  it("accepts a valid signature within the replay window", async () => {
    const sig = await signSlackPayload(secret, ts, body);
    expect(await verifySlackSignature(secret, body, sig, String(ts), ts + 30)).toBe(true);
  });

  it("rejects when the timestamp is outside the replay window", async () => {
    const sig = await signSlackPayload(secret, ts, body);
    expect(await verifySlackSignature(secret, body, sig, String(ts), ts + 60 * 60)).toBe(false);
  });

  it("rejects when the prefix is missing", async () => {
    const sig = await signSlackPayload(secret, ts, body);
    expect(
      await verifySlackSignature(secret, body, sig.replace("v0=", ""), String(ts), ts)
    ).toBe(false);
  });

  it("rejects when the timestamp header is missing", async () => {
    const sig = await signSlackPayload(secret, ts, body);
    expect(await verifySlackSignature(secret, body, sig, null, ts)).toBe(false);
  });

  it("rejects a non-numeric timestamp", async () => {
    const sig = await signSlackPayload(secret, ts, body);
    expect(await verifySlackSignature(secret, body, sig, "not-a-number", ts)).toBe(false);
  });
});

describe("verifyGenericSignature", () => {
  const secret = "shhh";
  const body = "ping";

  it("accepts the bare hex form", async () => {
    const sig = await signGenericPayload(secret, body);
    expect(await verifyGenericSignature(secret, body, sig)).toBe(true);
  });

  it("accepts the sha256= prefixed form", async () => {
    const sig = await signGenericPayload(secret, body);
    expect(await verifyGenericSignature(secret, body, `sha256=${sig}`)).toBe(true);
  });

  it("rejects a wrong-secret signature", async () => {
    const sig = await signGenericPayload("other-secret", body);
    expect(await verifyGenericSignature(secret, body, sig)).toBe(false);
  });

  it("rejects null signature", async () => {
    expect(await verifyGenericSignature(secret, body, null)).toBe(false);
  });
});
