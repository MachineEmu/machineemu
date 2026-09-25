/** Typed browser client for the Rust API v2 contract. */

import type { components } from "./api-types";

export type Profile = Record<string, unknown>;
export type Image = Record<string, unknown>;
export type CatalogProfile = Profile;
export type HealthResponse = { ok: boolean; api: string };
export type Instance = Record<string, unknown>;
export type CatalogSessionRequest = { profile_id: string; image_id: string; instance_id: string; auto_remove?: boolean };
export type CatalogSessionResponse = { session_id: string; instance_id: string; state: string; instance: Instance };
export type SessionSummary = {
  session_id: string; instance_id: string; profile_id: string; image_id: string;
  state: string; revision: number; ip?: string | null; configured: boolean; auto_remove: boolean;
};
export type StreamTicket = components["schemas"]["StreamTicket"];
export type TicketRequest = components["schemas"]["TicketRequest"];
export type CreateInstanceRequest = components["schemas"]["CreateInstance"];

export interface MachineEmuClientOptions { baseUrl?: string; token: string; fetchImpl?: typeof fetch }

export class MachineEmuApiError extends Error {
  constructor(readonly status: number, message: string) { super(message); this.name = "MachineEmuApiError"; }
}

export class MachineEmuClient {
  private readonly baseUrl: string;
  private readonly token: string;
  private readonly fetchImpl: typeof fetch;

  constructor(options: MachineEmuClientOptions) {
    this.baseUrl = (options.baseUrl ?? "").replace(/\/$/, "");
    this.token = options.token;
    this.fetchImpl = options.fetchImpl ?? ((input, init) => globalThis.fetch(input, init));
  }

  async health(): Promise<HealthResponse> { return this.request("/api/v2/health"); }
  async listProfiles(): Promise<CatalogProfile[]> { return this.request("/api/v2/profiles"); }
  async getProfile(profileId: string): Promise<CatalogProfile> { return this.request(`/api/v2/profiles/${encodeURIComponent(profileId)}`); }
  async listImages(): Promise<Image[]> { return this.request("/api/v2/images"); }

  async createInstance(input: CreateInstanceRequest): Promise<Instance> {
    return this.request("/api/v2/instances", { method: "POST", body: input });
  }

  async createCatalogSession(input: CatalogSessionRequest): Promise<CatalogSessionResponse> {
    const instance = await this.createInstance(input);
    const id = stringValue(instance, "instance_id") ?? input.instance_id;
    return { session_id: id, instance_id: id, state: stringValue(instance, "state") ?? "created", instance };
  }

  async listSessions(): Promise<SessionSummary[]> {
    const values = await this.request<unknown[]>("/api/v2/instances");
    return values.map((value) => {
      const item = objectValue(value); const instance = objectValue(item.instance);
      return {
        session_id: stringValue(instance, "instance_id") ?? "unknown",
        instance_id: stringValue(instance, "instance_id") ?? "unknown",
        profile_id: stringValue(instance, "profile_id") ?? "unknown",
        image_id: stringValue(instance, "image_id") ?? "unknown",
        state: stringValue(instance, "state") ?? "unknown",
        revision: numberValue(instance, "revision") ?? 0,
        ip: stringValue(item, "ip") ?? null, configured: item.configured === true, auto_remove: item.auto_remove === true,
      };
    });
  }

  async inspectSession(instanceId: string, _sessionId = instanceId): Promise<Instance> {
    return this.request(`/api/v2/instances/${encodeURIComponent(instanceId)}`);
  }

  async instanceConfig(instanceId: string): Promise<Record<string, unknown>> {
    return this.request(`/api/v2/instances/${encodeURIComponent(instanceId)}/config`);
  }

  async reconcile(): Promise<unknown> { return this.request("/api/v2/reconcile", { method: "POST" }); }
  async reconcileSession(_request: { instance_id: string; session_id?: string }): Promise<{ state: string }> {
    const result = objectValue(await this.reconcile()); return { state: stringValue(result, "state") ?? "reconciled" };
  }
  async startSession(instanceId: string, _sessionId = instanceId): Promise<{ state: string }> { return this.lifecycle(instanceId, "start"); }
  async stopSession(instanceId: string, _sessionId = instanceId): Promise<{ state: string }> { return this.lifecycle(instanceId, "stop"); }
  async sessionAction(instanceId: string, _sessionId: string, action: "pause" | "resume" | "reset"): Promise<{ state: string }> { return this.lifecycle(instanceId, action); }

  async screenshot(instanceId: string, _sessionId = instanceId): Promise<Blob> {
    return this.requestBlob(`/api/v2/instances/${encodeURIComponent(instanceId)}/screenshot`, { method: "POST" });
  }

  async streamTicket(instanceId: string, kind: string, request: TicketRequest = {}): Promise<StreamTicket> {
    return this.request(`/api/v2/instances/${encodeURIComponent(instanceId)}/streams/${encodeURIComponent(kind)}/ticket`, { method: "POST", body: request });
  }
  async createTerminalTicket(instanceId: string, _sessionId = instanceId): Promise<StreamTicket> { return this.streamTicket(instanceId, "serial", { control: true }); }

  private async lifecycle(instanceId: string, action: "start" | "stop" | "pause" | "resume" | "reset"): Promise<{ state: string }> {
    const value = objectValue(await this.request(`/api/v2/instances/${encodeURIComponent(instanceId)}/${action}`, { method: "POST", body: action === "start" ? {} : undefined }));
    return { state: stringValue(value, "state") ?? action };
  }

  private async request<T>(path: string, options: { method?: string; body?: unknown } = {}): Promise<T> {
    const headers = new Headers({ Accept: "application/json" });
    if (this.token) headers.set("Authorization", `Bearer ${this.token}`);
    if (options.body !== undefined) headers.set("Content-Type", "application/json");
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, { method: options.method ?? "GET", headers, body: options.body === undefined ? undefined : JSON.stringify(options.body) });
    if (!response.ok) throw new MachineEmuApiError(response.status, await errorText(response));
    return await response.json() as T;
  }

  private async requestBlob(path: string, options: { method?: string } = {}): Promise<Blob> {
    const headers = new Headers({ Accept: "image/png" });
    if (this.token) headers.set("Authorization", `Bearer ${this.token}`);
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, { method: options.method ?? "GET", headers });
    if (!response.ok) throw new MachineEmuApiError(response.status, await errorText(response));
    return response.blob();
  }
}

function objectValue(value: unknown): Record<string, unknown> { return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : {}; }
function stringValue(value: Record<string, unknown>, key: string): string | undefined { return typeof value[key] === "string" ? value[key] as string : undefined; }
function numberValue(value: Record<string, unknown>, key: string): number | undefined { return typeof value[key] === "number" ? value[key] as number : undefined; }
async function errorText(response: Response): Promise<string> {
  try { const value = objectValue(await response.json()); return stringValue(value, "error") ?? "The API request failed."; }
  catch { return "The API request failed."; }
}
