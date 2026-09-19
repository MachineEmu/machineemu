import { type FormEvent, useCallback, useEffect, useMemo, useState } from "react";
import { Link, Route, Routes, useNavigate, useParams, useSearchParams } from "react-router";
import { createCatalogSession } from "./catalog-flow";
import { MachineEmuClient, type CatalogProfile } from "./client";

function client(): MachineEmuClient {
  const token = document.querySelector<HTMLMetaElement>('meta[name="machineemu-token"]')?.content ?? "";
  return new MachineEmuClient({ token });
}

function errorMessage(reason: unknown, fallback: string): string {
  return reason instanceof Error ? reason.message : fallback;
}

function Header({ health }: { health?: boolean }) {
  const label = health === undefined ? "Checking service…" : health ? "Service ready" : "Service unavailable";
  const className = health === undefined ? "" : health ? "state-ready" : "state-unavailable";
  return <header className="machineemu-header">
    <p className="eyebrow">LOCAL EMULATION</p>
    <h1>MachineEmu</h1>
    <p className={`service-state ${className}`} role="status">{label}</p>
  </header>;
}

function ErrorNotice({ children }: { children: string }) {
  return <p className="notice notice-error" role="alert">{children}</p>;
}

function Catalog() {
  const api = useMemo(client, []);
  const navigate = useNavigate();
  const [profiles, setProfiles] = useState<CatalogProfile[]>([]);
  const [healthy, setHealthy] = useState<boolean>();
  const [error, setError] = useState<string>();
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(async () => {
    setLoading(true);
    setError(undefined);
    const [profileResult, healthResult] = await Promise.allSettled([api.listProfiles(), api.health()]);
    setHealthy(healthResult.status === "fulfilled");
    if (profileResult.status === "fulfilled") setProfiles(profileResult.value);
    else setError(errorMessage(profileResult.reason, "Unable to load catalog profiles."));
    setLoading(false);
  }, [api]);

  useEffect(() => { void reload(); }, [reload]);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setError(undefined);
    const form = new FormData(event.currentTarget);
    const instanceId = String(form.get("instance") ?? "").trim();
    const sessionId = String(form.get("session") ?? "").trim();
    try {
      const result = await createCatalogSession(api, {
        profileId: String(form.get("profile") ?? ""), instanceId, sessionId,
      });
      navigate(`/sessions/${encodeURIComponent(result.session.session_id)}?instance=${encodeURIComponent(instanceId)}`);
    } catch (reason) {
      setError(errorMessage(reason, "Unable to create session."));
    } finally {
      setBusy(false);
    }
  }

  return <main className="machineemu-page">
    <Header health={healthy} />
    <section className="panel" aria-labelledby="create-session-title">
      <h2 id="create-session-title">Create a session</h2>
      <p className="session-meta">Select an advertised profile. Host file paths are never entered in the browser.</p>
      <form onSubmit={submit}>
        <label className="field">Profile
          <select name="profile" required disabled={loading || !profiles.length}>
            {!profiles.length && <option value="">{loading ? "Loading profiles…" : "No profiles available"}</option>}
            {profiles.map((profile) => <option key={String(profile.id)} value={String(profile.id)}>
              {String(profile.id)} ({String(profile.machine)})
            </option>)}
          </select>
        </label>
        <label className="field">Instance ID<input name="instance" required maxLength={64} /></label>
        <label className="field">Session ID<input name="session" required maxLength={64} /></label>
        <div className="actions">
          <button className="button" disabled={busy || loading || !profiles.length}>
            {busy ? "Creating…" : "Create session"}
          </button>
          <button className="button button-secondary" type="button" disabled={loading || busy} onClick={() => void reload()}>
            Refresh catalog
          </button>
        </div>
      </form>
      {error ? <ErrorNotice>{error}</ErrorNotice> : !loading && !profiles.length ? <p className="notice">The service has no catalog profiles.</p> : null}
    </section>
  </main>;
}

function Session() {
  const api = useMemo(client, []);
  const { id = "" } = useParams();
  const [query] = useSearchParams();
  const instance = query.get("instance") ?? "";
  const [state, setState] = useState<string>();
  const [files, setFiles] = useState<number>();
  const [error, setError] = useState<string>();
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const validRoute = Boolean(instance && id);

  const refresh = useCallback(async () => {
    if (!validRoute) return;
    setLoading(true);
    setError(undefined);
    try {
      const [session, inventory] = await Promise.all([
        api.inspectSession(instance, id), api.inventoryInstanceState(instance),
      ]);
      setState(String(session.state));
      setFiles(Number(inventory.file_count));
    } catch (reason) {
      setError(errorMessage(reason, "Unable to inspect session."));
    } finally {
      setLoading(false);
    }
  }, [api, id, instance, validRoute]);

  useEffect(() => { void refresh(); }, [refresh]);

  async function action(kind: "reconcile" | "start" | "stop") {
    setBusy(true);
    setError(undefined);
    try {
      const result = kind === "reconcile"
        ? await api.reconcileSession({ instance_id: instance, session_id: id })
        : kind === "start"
          ? await api.startSession(instance, id)
          : await api.stopSession(instance, id);
      setState(String(result.state));
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason, "Session action failed."));
    } finally {
      setBusy(false);
    }
  }

  return <main className="machineemu-page">
    <Link className="back-link" to="/">← Catalog</Link>
    <Header />
    <section className="panel" aria-labelledby="session-title">
      <h2 id="session-title">Session</h2>
      {!validRoute ? <ErrorNotice>The session URL must include an instance ID.</ErrorNotice> : <>
        <p className="session-meta"><strong>{id}</strong> · {loading ? "Loading…" : state ?? "Unknown state"}</p>
        <p className="session-meta">Instance: {instance}{files !== undefined && ` · ${files} managed state files`}</p>
        <div className="actions">
          <button className="button button-secondary" disabled={busy || loading} onClick={() => void action("reconcile")}>
            {busy ? "Working…" : "Recover status"}
          </button>
          <button className="button" disabled={busy || loading || state === "running"} onClick={() => void action("start")}>
            {busy ? "Working…" : "Start"}
          </button>
          <button className="button button-secondary" disabled={busy || loading || state !== "running"} onClick={() => void action("stop")}>
            Stop
          </button>
          <button className="button button-secondary" disabled={busy || loading} onClick={() => void refresh()}>Refresh status</button>
        </div>
        {error && <ErrorNotice>{error}</ErrorNotice>}
      </>}
    </section>
  </main>;
}

function NotFound() {
  return <main className="machineemu-page"><Header /><section className="panel"><h2>Page not found</h2><p><Link to="/">Return to the catalog</Link>.</p></section></main>;
}

export function App() {
  return <Routes>
    <Route path="/" element={<Catalog />} />
    <Route path="/sessions/:id" element={<Session />} />
    <Route path="*" element={<NotFound />} />
  </Routes>;
}
