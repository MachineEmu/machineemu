import type { CatalogProfile, CatalogSessionResponse, MachineEmuClient } from "./client";

export interface CatalogSessionInput {
  profileId: string;
  instanceId: string;
  sessionId: string;
}

export interface CatalogSessionFlowResult {
  profile: CatalogProfile;
  session: CatalogSessionResponse;
}

/** Select a server-advertised profile and create a session without host paths. */
export async function createCatalogSession(
  client: Pick<MachineEmuClient, "listProfiles" | "createCatalogSession">,
  input: CatalogSessionInput,
): Promise<CatalogSessionFlowResult> {
  const profileId = input.profileId.trim();
  const instanceId = input.instanceId.trim();
  const sessionId = input.sessionId.trim();
  if (!profileId || !instanceId || !sessionId) {
    throw new Error("Profile, instance ID, and session ID are required.");
  }
  const profiles = await client.listProfiles();
  const profile = profiles.find((candidate) => candidate.id === profileId);
  if (!profile) throw new Error(`catalog profile not found: ${profileId}`);
  const session = await client.createCatalogSession({
    profile_id: profileId,
    instance_id: instanceId,
    session_id: sessionId,
  });
  return { profile, session };
}
