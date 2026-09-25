import type { CatalogProfile, CatalogSessionResponse, MachineEmuClient } from "./client";
import { profileId as profileDocumentId } from "./profile-detail";

export interface CatalogSessionInput {
  profileId: string;
  imageId?: string;
  instanceId: string;
  sessionId: string;
}

export interface CatalogSessionFlowResult {
  profile: CatalogProfile;
  session: CatalogSessionResponse;
}

/** Select a server-advertised profile and create a session without host paths. */
export async function createCatalogSession(
  client: Pick<MachineEmuClient, "listProfiles" | "createCatalogSession"> & Partial<Pick<MachineEmuClient, "listImages">>,
  input: CatalogSessionInput,
): Promise<CatalogSessionFlowResult> {
  const profileId = input.profileId.trim();
  const instanceId = input.instanceId.trim();
  const imageId = input.imageId?.trim() ?? (client.listImages ? String((await client.listImages())[0]?.image_id ?? "") : "");
  const sessionId = input.sessionId.trim() || instanceId;
  if (!profileId || !imageId || !instanceId) {
    throw new Error("Profile, image, and instance ID are required.");
  }
  const profiles = await client.listProfiles();
  const profile = profiles.find((candidate) => profileDocumentId(candidate) === profileId);
  if (!profile) throw new Error(`catalog profile not found: ${profileId}`);
  const session = await client.createCatalogSession({
    profile_id: profileId,
    image_id: imageId,
    instance_id: instanceId,
  });
  return { profile, session };
}
