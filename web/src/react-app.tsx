import { FormEvent, useEffect, useMemo, useState } from "react";
import { Link, Route, Routes, useNavigate } from "react-router";
import { MachineEmuClient, type CatalogProfile } from "./client";
import { createCatalogSession } from "./catalog-flow";

function client(): MachineEmuClient {
  return new MachineEmuClient({ token: document.querySelector<HTMLMetaElement>('meta[name="machineemu-token"]')?.content ?? "" });
}

function Catalog() {
  const api = useMemo(client, []);
  const navigate = useNavigate();
  const [profiles, setProfiles] = useState<CatalogProfile[]>([]);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  useEffect(() => { void api.listProfiles().then(setProfiles).catch((reason: unknown) => setError(reason instanceof Error ? reason.message : "Unable to load profiles.")); }, [api]);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setBusy(true); setError(undefined);
    const form = new FormData(event.currentTarget);
    try {
      const result = await createCatalogSession(api, { profileId: String(form.get("profile")), instanceId: String(form.get("instance")), sessionId: String(form.get("session")) });
      navigate(`/sessions/${encodeURIComponent(result.session.session_id)}`);
    } catch (reason) { setError(reason instanceof Error ? reason.message : "Unable to create session."); }
    finally { setBusy(false); }
  }
  return <main className="machineemu-catalog"><header><p>LOCAL EMULATION</p><h1>MachineEmu</h1></header><form onSubmit={submit}><label>Profile<select name="profile" required>{profiles.map((profile) => <option key={String(profile.id)} value={String(profile.id)}>{String(profile.id)} ({String(profile.machine)})</option>)}</select></label><label>Instance ID<input name="instance" required /></label><label>Session ID<input name="session" required /></label><button disabled={busy || !profiles.length}>{busy ? "Creating…" : "Create session"}</button></form>{error ? <p role="alert">{error}</p> : <p role="status">{profiles.length ? "Choose a profile." : "Loading profiles…"}</p>}</main>;
}
function Session() { return <main className="machineemu-catalog"><Link to="/">← Catalog</Link><h1>Session registered</h1><p>Session controls will migrate with the corresponding runtime API contract.</p></main>; }
export function App() { return <Routes><Route path="/" element={<Catalog />} /><Route path="/sessions/:id" element={<Session />} /><Route path="*" element={<Catalog />} /></Routes>; }
