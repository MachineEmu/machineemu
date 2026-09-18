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
