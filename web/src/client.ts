/** Small browser client for the initial MachineEmu session contract. */

import type { paths } from "./api-types";

export type SessionRequest = paths["/api/v1/sessions/reconcile"]["post"]["requestBody"]["content"]["application/json"];
export type HealthResponse = paths["/api/v1/health"]["get"]["responses"][200]["content"]["application/json"];
export type SessionManifest = paths["/api/v1/sessions/{instance_id}/{session_id}"]["get"]["responses"][200]["content"]["application/json"];
export type HardwareConfig = paths["/api/v1/sessions/{instance_id}/{session_id}/hardware-config"]["get"]["responses"][200]["content"]["application/json"];
export type EnvironmentReport = paths["/api/v1/sessions/{instance_id}/{session_id}/environment"]["get"]["responses"][200]["content"]["application/json"];
export type ReconcileResponse = paths["/api/v1/sessions/reconcile"]["post"]["responses"][200]["content"]["application/json"];
export type StartResponse = paths["/api/v1/sessions/{instance_id}/{session_id}/start"]["post"]["responses"][200]["content"]["application/json"];
export type StopResponse = paths["/api/v1/sessions/{instance_id}/{session_id}/stop"]["post"]["responses"][200]["content"]["application/json"];
export type OperationResponse = Record<string, unknown> & { operation_id: string; state: string };
export type CatalogProfile = paths["/api/v1/catalog/profiles"]["get"]["responses"][200]["content"]["application/json"][number];
export type CatalogSessionRequest = paths["/api/v1/catalog/sessions"]["post"]["requestBody"]["content"]["application/json"];
export type CatalogSessionResponse = paths["/api/v1/catalog/sessions"]["post"]["responses"][201]["content"]["application/json"];
export type DeviceInventory = paths["/api/v1/devices"]["get"]["responses"][200]["content"]["application/json"];
export type StateInventory = paths["/api/v1/instances/{instance_id}/state/inventory"]["get"]["responses"][200]["content"]["application/json"];
export type SnapshotInventory = { schema_version: number; snapshots: Array<{ snapshot_id: string; files: string[] }> };
export type SessionInventory = paths["/api/v1/sessions"]["get"]["responses"][200]["content"]["application/json"];
export type SessionSummary = SessionInventory["sessions"][number];
export type TerminalTicket = paths["/api/v1/sessions/{instance_id}/{session_id}/terminal/ticket"]["post"]["responses"][200]["content"]["application/json"];
export type QmpStatus = paths["/api/v1/sessions/{instance_id}/{session_id}/qmp/status"]["get"]["responses"][200]["content"]["application/json"];
export type QmpInspectRequest = paths["/api/v1/sessions/{instance_id}/{session_id}/qmp/inspect"]["post"]["requestBody"]["content"]["application/json"];
export type QmpInspectResponse = paths["/api/v1/sessions/{instance_id}/{session_id}/qmp/inspect"]["post"]["responses"][200]["content"]["application/json"];
export type ScreenshotResponse = Blob;
export type AudioStatus = {
  schema_version: number;
  available: boolean;
  reason?: string | null;
  capture_held: boolean;
  capture_ttl: number;
};
export type RemoteDeviceCapabilities = {
  schema_version: number;
  modes: Record<string, Record<string, unknown>>;
  profiles: Record<string, Record<string, unknown>>;
  limits: Record<string, number>;
};
export type RemoteDeviceAttachment = {
  attachment_id: string;
  session_id: string;
  mode: string;
  profile: string;
  selected_device?: Record<string, unknown> | null;
  state: string;
  generation: number;
  lease_deadline: number;
};
export type AudioControlResponse = Record<string, boolean | number | string>;
export type SessionAction = "pause" | "resume" | "reset";
export type LcdTouch = { screen: string } | { port: number } | { screensaver: boolean } | { dismiss: string };
export type VncControl = { action: "claim" | "release"; client_id: string; takeover?: boolean };
export type ExternalVnc = { ok: boolean; enabled: boolean; port?: number; password?: string; url?: string };
export type HwsimMedium = Record<string, number | boolean | null>;
export type BluetoothPeer = { address: string; data?: string; rssi?: number; event_type?: number; address_type?: number };


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
    // Calling a detached `fetch` with the client as `this` throws "Illegal invocation" in browsers.
    this.fetchImpl = options.fetchImpl ?? ((input, init) => globalThis.fetch(input, init));
  }

  async health(): Promise<HealthResponse> {
    return this.request("/api/v1/health");
  }

  async listProfiles(): Promise<CatalogProfile[]> {
    return this.request("/api/v1/catalog/profiles");
  }

  async listDevices(): Promise<DeviceInventory["devices"]> {
    return (await this.request<DeviceInventory>("/api/v1/devices")).devices;
  }

  async inspectDevice(deviceId: string): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/devices/${encodeURIComponent(deviceId)}`);
  }

  async validateDevice(deviceId: string, target?: string): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/devices/${encodeURIComponent(deviceId)}/launch-validation`, {
      method: "POST", body: target ? { target } : {},
    });
  }

  async createDeviceSession(deviceId: string, instanceId: string, sessionId: string, target?: string): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/devices/${encodeURIComponent(deviceId)}/sessions`, {
      method: "POST", body: { instance_id: instanceId, session_id: sessionId, ...(target ? { target } : {}) },
    });
  }

  async createAnalysisClone(deviceId: string, cloneId: string, target?: string): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/devices/${encodeURIComponent(deviceId)}/clones`, {
      method: "POST", body: { clone_id: cloneId, ...(target ? { target } : {}) },
    });
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

  async hardwareConfig(instanceId: string, sessionId: string): Promise<HardwareConfig> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/hardware-config`);
  }

  async environment(instanceId: string, sessionId: string): Promise<EnvironmentReport> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/environment`);
  }

  async logs(instanceId: string, sessionId: string, stream: "stdout" | "stderr" = "stdout", tail = 200): Promise<{ schema_version: number; stream: string; lines: string[]; truncated: boolean }> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/logs?stream=${stream}&tail=${tail}`);
  }

  async inventoryInstanceState(instanceId: string): Promise<StateInventory> {
    return this.request(`/api/v1/instances/${encodeURIComponent(instanceId)}/state/inventory`);
  }

  async listSnapshots(instanceId: string): Promise<SnapshotInventory> {
    return this.request(`/api/v1/instances/${encodeURIComponent(instanceId)}/snapshots`);
  }

  async createSnapshot(instanceId: string, snapshotId: string, files?: string[]): Promise<{ snapshot_id: string; state: string; files: string[] }> {
    return this.request(`/api/v1/instances/${encodeURIComponent(instanceId)}/snapshots`, {
      method: "POST", body: { snapshot_id: snapshotId, ...(files ? { files } : {}) },
    });
  }

  async restoreSnapshot(instanceId: string, snapshotId: string): Promise<{ snapshot_id: string; state: string }> {
    return this.request(`/api/v1/instances/${encodeURIComponent(instanceId)}/snapshots/${encodeURIComponent(snapshotId)}/restore`, {
      method: "POST",
    });
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

  async sessionAction(instanceId: string, sessionId: string, action: SessionAction): Promise<{ session_id: string; action: SessionAction; state: string }> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/actions`, {
      method: "POST", body: { action },
    });
  }

  async lcdTouch(instanceId: string, sessionId: string, touch: LcdTouch): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/lcd/touch`, {
      method: "POST", body: touch,
    });
  }

  async qmpInspect(instanceId: string, sessionId: string, request: QmpInspectRequest): Promise<QmpInspectResponse> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/qmp/inspect`, {
      method: "POST", body: request,
    });
  }

  async screenshot(instanceId: string, sessionId: string): Promise<ScreenshotResponse> {
    return this.requestBlob(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/screenshot`);
  }

  async vncControl(instanceId: string, sessionId: string, request: VncControl): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/vnc/control`, {
      method: "POST", body: request,
    });
  }

  async vncExposeStatus(instanceId: string, sessionId: string): Promise<ExternalVnc> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/vnc/expose`);
  }

  async vncExpose(instanceId: string, sessionId: string): Promise<ExternalVnc> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/vnc/expose`, { method: "POST" });
  }

  async vncExposeStop(instanceId: string, sessionId: string): Promise<{ ok: boolean; enabled: boolean }> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/vnc/expose`, { method: "DELETE" });
  }

  async hwsimStatus(instanceId: string, sessionId: string): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/hwsim`);
  }

  async hwsimMedium(instanceId: string, sessionId: string, settings: HwsimMedium): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/hwsim/medium`, { method: "POST", body: settings });
  }

  async bluetoothStatus(instanceId: string, sessionId: string): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/bluetooth`);
  }

  async bluetoothAdvertise(instanceId: string, sessionId: string, peer: BluetoothPeer): Promise<Record<string, unknown>> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/bluetooth/advertise`, { method: "POST", body: peer });
  }

  async audioStatus(instanceId: string, sessionId: string): Promise<AudioStatus> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/audio`);
  }

  async audioControl(instanceId: string, sessionId: string, request: {
    action: "attach" | "renew" | "claim" | "release" | "detach";
    client_token?: string;
    takeover?: boolean;
  }): Promise<AudioControlResponse> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/audio/control`, {
      method: "POST", body: request,
    });
  }

  async remoteDeviceCapabilities(instanceId: string, sessionId: string): Promise<RemoteDeviceCapabilities> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/remote-devices/capabilities`);
  }

  async listRemoteDeviceAttachments(instanceId: string, sessionId: string): Promise<RemoteDeviceAttachment[]> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/remote-devices/attachments`);
  }

  async createRemoteDeviceAttachment(instanceId: string, sessionId: string, mode: "generic_usb" | "webauthn" | "ctap", profile: string): Promise<RemoteDeviceAttachment> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/remote-devices/attachments`, {
      method: "POST", body: { mode, profile },
    });
  }

  async remoteDeviceConnectTicket(instanceId: string, sessionId: string, attachmentId: string, role: "local" | "guest"): Promise<{ ticket: string }> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/remote-devices/attachments/${encodeURIComponent(attachmentId)}/connect-ticket`, {
      method: "POST", body: { role },
    });
  }

  async revokeRemoteDeviceAttachment(instanceId: string, sessionId: string, attachmentId: string): Promise<RemoteDeviceAttachment> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/remote-devices/attachments/${encodeURIComponent(attachmentId)}`, {
      method: "DELETE",
    });
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

  async restartSession(instanceId: string, sessionId: string): Promise<OperationResponse> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/restart`, { method: "POST" });
  }

  async deleteSession(instanceId: string, sessionId: string): Promise<OperationResponse> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}`, { method: "DELETE" });
  }

  async operation(operationId: string): Promise<OperationResponse> {
    return this.request(`/api/v1/operations/${encodeURIComponent(operationId)}`);
  }

  async hostUsb(): Promise<{ devices: Array<Record<string, unknown>> }> {
    return this.request("/api/v1/host/usb");
  }

  async usbStatus(instanceId: string, sessionId: string): Promise<{ devices: Array<Record<string, unknown>> }> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/usb`);
  }

  async usbAttachHost(instanceId: string, sessionId: string, hostbus: number, hostaddr: number): Promise<OperationResponse> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/usb/host`, { method: "POST", body: { hostbus, hostaddr } });
  }

  async usbAttachImage(instanceId: string, sessionId: string, image: string, readOnly = true): Promise<OperationResponse> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/usb/image`, { method: "POST", body: { image, read_only: readOnly } });
  }

  async usbDetach(instanceId: string, sessionId: string, id: string): Promise<OperationResponse> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/usb/detach`, { method: "POST", body: { id } });
  }

  async cdromStatus(instanceId: string, sessionId: string): Promise<{ devices: Array<Record<string, unknown>> }> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/cdrom`);
  }

  async networkStatus(instanceId: string, sessionId: string): Promise<{ devices: Array<Record<string, unknown>> }> {
    return this.request(`/api/v1/sessions/${encodeURIComponent(instanceId)}/${encodeURIComponent(sessionId)}/network`);
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

  private async requestBlob(path: string): Promise<Blob> {
    const headers = new Headers({ "X-MachineEmu-Token": this.token, Accept: "image/*" });
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, { headers });
    if (!response.ok) {
      throw new MachineEmuApiError(response.status, `MachineEmu API request failed (${response.status}).`);
    }
    return response.blob();
  }
}
