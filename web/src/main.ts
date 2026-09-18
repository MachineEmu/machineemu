import { MachineEmuClient } from "./client";
import { createCatalogSession } from "./catalog-flow";

export interface CatalogScreenClient {
  listProfiles: MachineEmuClient["listProfiles"];
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
  form.append(profile, instance, session, submit);
  root.append(heading, form, status);

  try {
    const profiles = await client.listProfiles();
    for (const item of profiles) {
      const option = document.createElement("option");
      option.value = String(item.id);
      option.textContent = `${item.id} (${String(item.machine)})`;
      profile.append(option);
    }
    status.textContent = profiles.length ? "Choose a profile." : "No profiles are available.";
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
        instanceId: instance.value,
        sessionId: session.value,
      });
      status.textContent = `Created ${result.session.session_id} from ${result.profile.id}.`;
    } catch (error) {
      status.textContent = error instanceof Error ? error.message : "Unable to create session.";
    } finally {
      submit.disabled = false;
    }
  });
}

export function bootstrap(root: HTMLElement = document.body): void {
  const token = document.querySelector<HTMLMetaElement>('meta[name="machineemu-token"]')?.content ?? "";
  void mountCatalogScreen(root, new MachineEmuClient({ token }));
}

if (typeof document !== "undefined") bootstrap();
