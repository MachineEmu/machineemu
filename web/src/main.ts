import { MachineEmuClient } from "./client";
import { createCatalogSession } from "./catalog-flow";
import { profileId } from "./profile-detail";

export interface CatalogScreenClient {
  listProfiles: MachineEmuClient["listProfiles"];
  listImages: MachineEmuClient["listImages"];
  createCatalogSession: MachineEmuClient["createCatalogSession"];
}

export async function mountCatalogScreen(root: HTMLElement, client: CatalogScreenClient): Promise<void> {
  root.replaceChildren();
  const heading = document.createElement("h1");
  heading.textContent = "MachineEmu";
  const form = document.createElement("form");
  const profile = document.createElement("select");
  profile.name = "profile";
  const instance = document.createElement("input");
  instance.name = "instance_id";
  instance.placeholder = "Instance ID";
  instance.required = true;
  const session = document.createElement("input");
  session.name = "session_id";
  session.placeholder = "Session ID";
  session.required = true;
  const submit = document.createElement("button");
  submit.type = "submit";
  submit.textContent = "Create session";
  const status = document.createElement("p");
  status.setAttribute("role", "status");
  const image = document.createElement("select");
  image.name = "image_id";
  form.append(profile, image, instance, session, submit);
  root.append(heading, form, status);

  try {
    const [profiles, images] = await Promise.all([client.listProfiles(), client.listImages()]);
    for (const item of profiles) {
      const option = document.createElement("option");
      option.value = profileId(item);
      option.textContent = profileId(item);
      profile.append(option);
    }
    for (const item of images) {
      const option = document.createElement("option");
      option.value = String(item.image_id);
      option.textContent = `${item.image_id} (${String(item.target ?? "unknown target")})`;
      image.append(option);
    }
    status.textContent = profiles.length && images.length ? "Choose a profile and image." : "No profiles or images are available.";
  } catch (error) {
    status.textContent = error instanceof Error ? error.message : "Unable to load profiles.";
    return;
  }

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    submit.disabled = true;
    status.textContent = "Creating session…";
    try {
      const result = await createCatalogSession(client, {
        profileId: profile.value,
        imageId: image.value,
        instanceId: instance.value,
        sessionId: session.value,
      });
      status.textContent = `Created ${result.session.session_id} from ${profileId(result.profile)}.`;
    } catch (error) {
      status.textContent = error instanceof Error ? error.message : "Unable to create session.";
    } finally {
      submit.disabled = false;
    }
  });
}

export function bootstrap(root: HTMLElement = document.body): void {
  void mountCatalogScreen(root, new MachineEmuClient({ token: "" }));
}

if (typeof document !== "undefined") bootstrap();
