/** Small browser client for the initial MachineEmu session contract. */

export interface SessionRequest {
  instance_id: string;
  session_id: string;
}

export interface SessionManifest extends Record<string, unknown> {
  session_id: string;
  instance_id: string;
  state: string;
}

export interface MachineEmuClientOptions {
  baseUrl?: string;
  token: string;
  fetchImpl?: typeof fetch;
}

export class MachineEmuClient {
  private readonly baseUrl: string;
  private readonly token: string;
  private readonly fetchImpl: typeof fetch;

  constructor(options: MachineEmuClientOptions) {
    this.baseUrl = (options.baseUrl ?? "").replace(/\/$/, "");
    this.token = options.token;
    this.fetchImpl = options.fetchImpl ?? fetch;
  }

  async health(): Promise<{ status: string }> {
    return this.request("/api/v1/health");
  }

  async inspectSession(instanceId: string, sessionId: string): Promise<SessionManifest> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}`);
  }

  async reconcileSession(request: SessionRequest): Promise<{ session_id: string; state: string }> {
    return this.request("/api/v1/sessions/reconcile", { method: "POST", body: request });
  }

  async startSession(instanceId: string, sessionId: string): Promise<{ session_id: string; pid: number; state: string }> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/start`, {
      method: "POST",
    });
  }

  async stopSession(instanceId: string, sessionId: string): Promise<{ session_id: string; exit_code: number; state: string }> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/stop`, {
      method: "POST",
    });
  }

  private async request<T>(path: string, init: RequestInit & { body?: unknown } = {}): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set("X-MachineEmu-Token", this.token);
    headers.set("Accept", "application/json");
    let body = init.body;
    if (body !== undefined && typeof body !== "string") {
      headers.set("Content-Type", "application/json");
      body = JSON.stringify(body);
    }
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, { ...init, headers, body: body as BodyInit | null | undefined });
    if (!response.ok) throw new Error(`MachineEmu API request failed: ${response.status}`);
    return (await response.json()) as T;
  }
}
