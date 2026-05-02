import type { HelmEnvironment } from "./manifest.js";

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

export function requireAccess(request: Request, required: boolean): Response | null {
  if (!required) return null;

  const email = request.headers.get("Cf-Access-Authenticated-User-Email");
  const jwt = request.headers.get("Cf-Access-Jwt-Assertion");
  if (email || jwt) return null;

  return json(
    {
      error: "unauthorized",
      message: "Cloudflare Access authentication is required."
    },
    { status: 401 },
  );
}
