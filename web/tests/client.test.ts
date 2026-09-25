import { describe, expect, it } from "bun:test";

import { MachineEmuApiError, MachineEmuClient } from "../src/client";

function testClient(response: unknown, status = 200) {
  const calls: Request[] = [];
  const client = new MachineEmuClient({
    baseUrl: "http://127.0.0.1", token: "token",
    fetchImpl: async (input, init) => {
      calls.push(new Request(input, init));
      return new Response(JSON.stringify(response), { status, headers: { "Content-Type": "application/json" } });
    },
  });
  return { client, calls };
}

describe("MachineEmuClient", () => {
  it("uses bearer auth and the v2 health endpoint", async () => {
    const { client, calls } = testClient({ ok: true, api: "v2" });
    expect(await client.health()).toEqual({ ok: true, api: "v2" });
    expect(calls[0].url).toBe("http://127.0.0.1/api/v2/health");
    expect(calls[0].headers.get("Authorization")).toBe("Bearer token");
  });

  it("lists v2 instance status records and exposes network metadata", async () => {
    const { client } = testClient([{ instance: { instance_id: "instance", profile_id: "demo", image_id: "img", state: "running", revision: 2 }, ip: "192.0.2.10", configured: true, auto_remove: false }]);
    await expect(client.listSessions()).resolves.toEqual([{ session_id: "instance", instance_id: "instance", profile_id: "demo", image_id: "img", state: "running", revision: 2, ip: "192.0.2.10", configured: true, auto_remove: false }]);
  });

  it("creates an instance with a v2 JSON body", async () => {
    const { client, calls } = testClient({ instance_id: "instance", state: "created" });
    await client.createCatalogSession({ profile_id: "demo", image_id: "img", instance_id: "instance" });
    expect(calls[0].url).toBe("http://127.0.0.1/api/v2/instances");
    expect(calls[0].method).toBe("POST");
    expect(await calls[0].json()).toEqual({ profile_id: "demo", image_id: "img", instance_id: "instance" });
  });

  it("issues a ticket for the v2 serial stream", async () => {
    const { client, calls } = testClient({ ticket: "opaque", expires_in_seconds: 30, kind: "serial" });
    await client.createTerminalTicket("instance", "ignored");
    expect(calls[0].url).toBe("http://127.0.0.1/api/v2/instances/instance/streams/serial/ticket");
    expect(await calls[0].json()).toEqual({ control: true });
  });

  it("preserves the daemon error field", async () => {
    const { client } = testClient({ error: "instance not found" }, 404);
    try { await client.getProfile("missing"); throw new Error("expected request to fail"); }
    catch (error) {
      expect(error).toBeInstanceOf(MachineEmuApiError);
      expect((error as MachineEmuApiError).status).toBe(404);
      expect((error as Error).message).toBe("instance not found");
    }
  });
});
