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
  const profiles = await client.listProfiles();
  const profile = profiles.find((candidate) => candidate.id === input.profileId);
  if (!profile) throw new Error(`catalog profile not found: ${input.profileId}`);
  const session = await client.createCatalogSession({
    profile_id: input.profileId,
    instance_id: input.instanceId,
    session_id: input.sessionId,
  });
  return { profile, session };
}
