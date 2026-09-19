import { describe, expect, it } from "bun:test";

import { MachineEmuApiError, MachineEmuClient } from "../src/client";
import { createCatalogSession } from "../src/catalog-flow";

describe("MachineEmuClient", () => {
  it("sends authenticated read requests with encoded session IDs", async () => {
    const calls: Request[] = [];
    const client = new MachineEmuClient({
      baseUrl: "http://127.0.0.1",
      token: "token",
      fetchImpl: async (input, init) => {
        calls.push(new Request(input, init));
        return new Response(JSON.stringify({ session_id: "a/b", instance_id: "instance", state: "created" }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      },
    });

    const result = await client.inspectSession("instance", "a/b");
    expect(result.state).toBe("created");
    expect(calls[0].url).toBe("http://127.0.0.1/api/v1/sessions/instance/a%2Fb");
    expect(calls[0].headers.get("X-MachineEmu-Token")).toBe("token");
  });

  it("requests a read-only instance state inventory", async () => {
    const calls: Request[] = [];
    const client = new MachineEmuClient({
      baseUrl: "http://127.0.0.1", token: "token",
      fetchImpl: async (input, init) => {
        calls.push(new Request(input, init));
        return new Response(JSON.stringify({ schema_version: 1, files: [], file_count: 0 }), { status: 200 });
      },
    });
    const result = await client.inventoryInstanceState("instance/one");
    expect(result.file_count).toBe(0);
    expect(calls[0].url).toBe("http://127.0.0.1/api/v1/instances/instance%2Fone/state/inventory");
    expect(calls[0].method).toBe("GET");
  });

  it("lists public session summaries", async () => {
    const calls: Request[] = [];
    const client = new MachineEmuClient({
      baseUrl: "http://127.0.0.1", token: "token",
      fetchImpl: async (input, init) => {
        calls.push(new Request(input, init));
        return new Response(JSON.stringify({ sessions: [{
          session_id: "session", instance_id: "instance", profile_id: "demo", machine: "virt",
          state: "stopped", capabilities: {},
        }] }), { status: 200 });
      },
    });
    const sessions = await client.listSessions();
    expect(sessions[0].session_id).toBe("session");
    expect(calls[0].url).toBe("http://127.0.0.1/api/v1/sessions");
  });

  it("sends same-origin mutation bodies as JSON", async () => {
    const calls: Request[] = [];
    const client = new MachineEmuClient({
      baseUrl: "http://127.0.0.1",
      token: "token",
      fetchImpl: async (input, init) => {
        calls.push(new Request(input, init));
        return new Response(JSON.stringify({ session_id: "session", state: "failed" }), { status: 200 });
      },
    });

    await client.reconcileSession({ instance_id: "instance", session_id: "session" });
    expect(calls[0].method).toBe("POST");
    expect(calls[0].headers.get("Content-Type")).toBe("application/json");
    expect(await calls[0].json()).toEqual({ instance_id: "instance", session_id: "session" });
  });

  it("preserves a safe API error detail for the interface", async () => {
    const client = new MachineEmuClient({
      token: "token",
      fetchImpl: async () => new Response(JSON.stringify({ detail: "catalog is not configured" }), {
        status: 404, headers: { "Content-Type": "application/json" },
      }),
    });

    try {
      await client.listProfiles();
      throw new Error("expected request to fail");
    } catch (error) {
      expect(error).toBeInstanceOf(MachineEmuApiError);
      expect((error as MachineEmuApiError).status).toBe(404);
      expect((error as Error).message).toBe("catalog is not configured");
    }
  });

  it("uses catalog IDs for profile selection and session creation", async () => {
    const calls: Request[] = [];
    const client = new MachineEmuClient({
      baseUrl: "http://127.0.0.1",
      token: "token",
      fetchImpl: async (input, init) => {
        calls.push(new Request(input, init));
        return new Response(JSON.stringify({ id: "demo", machine: "virt" }), { status: 200 });
      },
    });

    await client.getProfile("demo/lab");
    await client.createCatalogSession({ profile_id: "demo", instance_id: "instance", session_id: "session" });
    expect(calls[0].url).toBe("http://127.0.0.1/api/v1/catalog/profiles/demo%2Flab");
    expect(calls[1].url).toBe("http://127.0.0.1/api/v1/catalog/sessions");
    expect(await calls[1].json()).toEqual({ profile_id: "demo", instance_id: "instance", session_id: "session" });
  });

  it("orchestrates catalog selection before session creation", async () => {
    const calls: string[] = [];
    const client = {
      listProfiles: async () => [{ id: "demo", machine: "virt" } as never],
      createCatalogSession: async (request: { profile_id: string; instance_id: string; session_id: string }) => {
        calls.push(`${request.profile_id}:${request.instance_id}:${request.session_id}`);
        return { session_id: request.session_id, manifest: "/runtime/manifest.json", state: "created" } as never;
      },
    };
    const result = await createCatalogSession(client, {
      profileId: "demo", instanceId: "instance", sessionId: "session",
    });
    expect(result.profile.id).toBe("demo");
    expect(result.session.state).toBe("created");
    expect(calls).toEqual(["demo:instance:session"]);
  });

  it("rejects empty catalog identifiers before making a request", async () => {
    const client = {
      listProfiles: async () => { throw new Error("must not be called"); },
      createCatalogSession: async () => { throw new Error("must not be called"); },
    } as never;
    await expect(createCatalogSession(client, {
      profileId: "demo", instanceId: "   ", sessionId: "session",
    })).rejects.toThrow("Profile, instance ID, and session ID are required.");
  });
});
