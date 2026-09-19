/** Small browser client for the initial MachineEmu session contract. */

import type { paths } from "./api-types";

export type SessionRequest = paths["/api/v1/sessions/reconcile"]["post"]["requestBody"]["content"]["application/json"];
export type HealthResponse = paths["/api/v1/health"]["get"]["responses"][200]["content"]["application/json"];
export type SessionManifest = paths["/api/v1/sessions/{instance_id}/{session_id}"]["get"]["responses"][200]["content"]["application/json"];
export type ReconcileResponse = paths["/api/v1/sessions/reconcile"]["post"]["responses"][200]["content"]["application/json"];
export type StartResponse = paths["/api/v1/sessions/{instance_id}/{session_id}/start"]["post"]["responses"][200]["content"]["application/json"];
export type StopResponse = paths["/api/v1/sessions/{instance_id}/{session_id}/stop"]["post"]["responses"][200]["content"]["application/json"];
export type CatalogProfile = paths["/api/v1/catalog/profiles"]["get"]["responses"][200]["content"]["application/json"][number];
export type CatalogSessionRequest = paths["/api/v1/catalog/sessions"]["post"]["requestBody"]["content"]["application/json"];
export type CatalogSessionResponse = paths["/api/v1/catalog/sessions"]["post"]["responses"][201]["content"]["application/json"];
export type StateInventory = paths["/api/v1/instances/{instance_id}/state/inventory"]["get"]["responses"][200]["content"]["application/json"];
export type SessionInventory = paths["/api/v1/sessions"]["get"]["responses"][200]["content"]["application/json"];
export type SessionSummary = SessionInventory["sessions"][number];
export type TerminalTicket = paths["/api/v1/sessions/{instance_id}/{session_id}/terminal/ticket"]["post"]["responses"][200]["content"]["application/json"];
export type QmpStatus = paths["/api/v1/sessions/{instance_id}/{session_id}/qmp/status"]["get"]["responses"][200]["content"]["application/json"];


export interface MachineEmuClientOptions {
  baseUrl?: string;
  token: string;
  fetchImpl?: typeof fetch;
}

/** An API response that the UI can safely present without guessing its cause. */
export class MachineEmuApiError extends Error {
  constructor(readonly status: number, message: string) {
    super(message);
    this.name = "MachineEmuApiError";
  }
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

  async health(): Promise<HealthResponse> {
    return this.request("/api/v1/health");
  }

  async listProfiles(): Promise<CatalogProfile[]> {
    return this.request("/api/v1/catalog/profiles");
  }

  async getProfile(profileId: string): Promise<CatalogProfile> {
    return this.request(`/api/v1/catalog/profiles/${encodeURIComponent(profileId)}`);
  }

  async createCatalogSession(request: CatalogSessionRequest): Promise<CatalogSessionResponse> {
    return this.request("/api/v1/catalog/sessions", { method: "POST", body: request });
  }

  async inspectSession(instanceId: string, sessionId: string): Promise<SessionManifest> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}`);
  }

  async inventoryInstanceState(instanceId: string): Promise<StateInventory> {
    return this.request(`/api/v1/instances/${encodeURIComponent(instanceId)}/state/inventory`);
  }

  async listSessions(): Promise<SessionSummary[]> {
    const inventory = await this.request<SessionInventory>("/api/v1/sessions");
    return inventory.sessions;
  }

  async createTerminalTicket(instanceId: string, sessionId: string): Promise<TerminalTicket> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/terminal/ticket`, {
      method: "POST", body: {},
    });
  }

  async qmpStatus(instanceId: string, sessionId: string): Promise<QmpStatus> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/qmp/status`);
  }

  async reconcileSession(request: SessionRequest): Promise<ReconcileResponse> {
    return this.request("/api/v1/sessions/reconcile", { method: "POST", body: request });
  }

  async startSession(instanceId: string, sessionId: string): Promise<StartResponse> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/start`, {
      method: "POST",
    });
  }

  async stopSession(instanceId: string, sessionId: string): Promise<StopResponse> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/stop`, {
      method: "POST",
    });
  }

  private async request<T>(path: string, init: { method?: string; headers?: HeadersInit; body?: unknown } = {}): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set("X-MachineEmu-Token", this.token);
    headers.set("Accept", "application/json");
    let body = init.body;
    if (body !== undefined && typeof body !== "string") {
      headers.set("Content-Type", "application/json");
      body = JSON.stringify(body);
    }
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, {
      method: init.method,
      headers,
      body: body as BodyInit | null | undefined,
    });
    if (!response.ok) {
      let message = `MachineEmu API request failed (${response.status}).`;
      try {
        const payload = await response.json() as { detail?: unknown };
        if (typeof payload.detail === "string" && payload.detail) message = payload.detail;
      } catch {
        // A proxy may return an empty or non-JSON error body. The status is still useful.
      }
      throw new MachineEmuApiError(response.status, message);
    }
    return (await response.json()) as T;
  }
}
