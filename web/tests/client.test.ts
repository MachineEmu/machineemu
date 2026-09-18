import { describe, expect, it } from "bun:test";

import { MachineEmuClient } from "../src/client";

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
});
