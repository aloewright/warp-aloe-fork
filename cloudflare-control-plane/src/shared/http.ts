import type { HelmEnvironment, HelmManifest } from "./manifest.js";

export interface HealthPayload {
  service: string;
  environment: HelmEnvironment;
  version: string;
  buildId: string;
}

export function json(data: unknown, init: ResponseInit = {}): Response {
  return new Response(JSON.stringify(data, null, 2), {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...init.headers
    }
  });
}

export function methodNotAllowed(): Response {
  return json({ error: "method_not_allowed" }, { status: 405 });
}

export function notFound(): Response {
  return json({ error: "not_found" }, { status: 404 });
}

export function health(payload: HealthPayload): Response {
  return json(payload);
}

interface AccessJwtHeader {
  alg?: string;
  kid?: string;
}

interface AccessJwtPayload {
  aud?: string | string[];
  exp?: number;
  nbf?: number;
  iss?: string;
}

interface AccessJwk extends JsonWebKey {
  kid?: string;
}

interface AccessCerts {
  keys?: AccessJwk[];
}

function base64UrlDecode(value: string): Uint8Array {
  const normalized = value.replace(/-/g, "+").replace(/_/g, "/");
  const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=");
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

function decodeJson<T>(value: string): T {
  const bytes = base64UrlDecode(value);
  return JSON.parse(new TextDecoder().decode(bytes)) as T;
}

function audienceMatches(actual: string | string[] | undefined, expected?: string): boolean {
  if (!expected) return true;
  if (Array.isArray(actual)) return actual.includes(expected);
  return actual === expected;
}

async function verifyAccessJwt(jwt: string, teamDomain: string, expectedAudience?: string): Promise<boolean> {
  const [headerPart, payloadPart, signaturePart] = jwt.split(".");
  if (!headerPart || !payloadPart || !signaturePart) return false;

  const header = decodeJson<AccessJwtHeader>(headerPart);
  if (header.alg !== "RS256" || !header.kid) return false;

  const payload = decodeJson<AccessJwtPayload>(payloadPart);
  const now = Math.floor(Date.now() / 1000);
  if (payload.exp != null && payload.exp <= now) return false;
  if (payload.nbf != null && payload.nbf > now) return false;
  if (!audienceMatches(payload.aud, expectedAudience)) return false;

  const certsUrl = `https://${teamDomain}/cdn-cgi/access/certs`;
  const certsResponse = await fetch(certsUrl);
  if (!certsResponse.ok) return false;
  const certs = (await certsResponse.json()) as AccessCerts;
  const jwk = certs.keys?.find((key) => key.kid === header.kid);
  if (!jwk) return false;

  const key = await crypto.subtle.importKey(
    "jwk",
    jwk,
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["verify"],
  );

  return crypto.subtle.verify(
    "RSASSA-PKCS1-v1_5",
    key,
    base64UrlDecode(signaturePart),
    new TextEncoder().encode(`${headerPart}.${payloadPart}`),
  );
}

export async function requireAccess(
  request: Request,
  access: HelmManifest["access"],
  environment: HelmEnvironment,
): Promise<Response | null> {
  if (!access.required) return null;

  const jwt = request.headers.get("Cf-Access-Jwt-Assertion");
  if (jwt) {
    try {
      if (await verifyAccessJwt(jwt, access.teamDomain, access.audiences[environment])) {
        return null;
      }
    } catch {
      return json(
        {
          error: "unauthorized",
          message: "Cloudflare Access JWT validation failed."
        },
        { status: 401 },
      );
    }
  }

  return json(
    {
      error: "unauthorized",
      message: "A valid Cloudflare Access JWT is required."
    },
    { status: 401 },
  );
}
