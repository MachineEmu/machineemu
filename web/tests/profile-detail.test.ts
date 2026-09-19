import { describe, expect, it } from "bun:test";

import { profileDetail } from "../src/profile-detail";

describe("profileDetail", () => {
  it("selects public capability metadata without copying unknown profile fields", () => {
    const detail = profileDetail({
      id: "udm-pro-lab", machine: "udm-pro", target: "aarch64-softmmu",
      resources: { memory: "2GiB", vcpus: 4, ignored: false },
      devices: { lcd: true, bluetooth: true, secret: false },
      network: { mode: "bridge", bridge: "br0" },
      external_assets: [{ id: "bundle", kind: "firmware", required: true, note: "Import it." }],
      private_path: "/host/private",
    } as never);
    expect(detail).toEqual({
      id: "udm-pro-lab", machine: "udm-pro", target: "aarch64-softmmu",
      resources: { memory: "2GiB", vcpus: 4 }, devices: ["lcd", "bluetooth"], networkMode: "bridge",
      assetRequirements: [{ id: "bundle", kind: "firmware", required: true, note: "Import it." }],
    });
  });
});
