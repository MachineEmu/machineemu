import { type FormEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, Route, Routes, useNavigate, useParams, useSearchParams } from "react-router";
import { createCatalogSession } from "./catalog-flow";
import { MachineEmuClient, type CatalogProfile, type SessionSummary } from "./client";
import { profileDetail, type ProfileDetail } from "./profile-detail";

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
          <Link className="button button-secondary" to="/sessions">Open sessions</Link>
          <button className="button button-secondary" type="button" disabled={loading || busy} onClick={() => void reload()}>
            Refresh catalog
          </button>
        </div>
      </form>
      {error ? <ErrorNotice>{error}</ErrorNotice> : !loading && !profiles.length ? <p className="notice">The service has no catalog profiles.</p> : null}
    </section>
    {!loading && profiles.length ? <section className="profile-list" aria-labelledby="profiles-title">
      <h2 id="profiles-title">Available profiles</h2>
      {profiles.map((profile) => <Link className="session-row" key={String(profile.id)} to={`/profiles/${encodeURIComponent(String(profile.id))}`}>
        <strong>{String(profile.id)}</strong><span>{String(profile.machine)}</span>
      </Link>)}
    </section> : null}
  </main>;
}

function DetailList({ title, values }: { title: string; values: Record<string, string | number> }) {
  const entries = Object.entries(values);
  if (!entries.length) return null;
  return <section className="detail-section"><h3>{title}</h3><dl>{entries.map(([name, value]) => <div key={name}><dt>{name}</dt><dd>{value}</dd></div>)}</dl></section>;
}

function Profile({ detail }: { detail: ProfileDetail }) {
  return <>
    <p className="session-meta">Machine: {detail.machine}{detail.target && ` · ${detail.target}`}</p>
    <DetailList title="Resources" values={detail.resources} />
    {detail.devices.length ? <section className="detail-section"><h3>Declared devices</h3><p>{detail.devices.join(", ")}</p></section> : null}
    {detail.networkMode ? <section className="detail-section"><h3>Network</h3><p>{detail.networkMode}</p></section> : null}
    {detail.assetRequirements.length ? <section className="detail-section"><h3>Required imports</h3><ul>{detail.assetRequirements.map((asset) => <li key={asset.id}><strong>{asset.id}</strong> · {asset.kind}{asset.required ? " · required" : ""}{asset.note && ` — ${asset.note}`}</li>)}</ul></section> : null}
  </>;
}

function ProfileRoute() {
  const api = useMemo(client, []);
  const { id = "" } = useParams();
  const [detail, setDetail] = useState<ProfileDetail>();
  const [error, setError] = useState<string>();
  const [loading, setLoading] = useState(true);
  const load = useCallback(async () => {
    setLoading(true); setError(undefined);
    try { setDetail(profileDetail(await api.getProfile(id))); }
    catch (reason) { setError(errorMessage(reason, "Unable to load profile.")); }
    finally { setLoading(false); }
  }, [api, id]);
  useEffect(() => { if (id) void load(); }, [id, load]);
  return <main className="machineemu-page">
    <Link className="back-link" to="/">← Catalog</Link><Header />
    <section className="panel" aria-labelledby="profile-title"><h2 id="profile-title">{loading ? "Loading profile…" : detail?.id ?? "Profile"}</h2>
      {error ? <ErrorNotice>{error}</ErrorNotice> : detail ? <Profile detail={detail} /> : null}
      <div className="actions"><button className="button button-secondary" disabled={loading} onClick={() => void load()}>Refresh</button></div>
    </section>
  </main>;
}

function Sessions() {
  const api = useMemo(client, []);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [error, setError] = useState<string>();
  const [loading, setLoading] = useState(true);
  const reload = useCallback(async () => {
    setLoading(true);
    setError(undefined);
    try {
      setSessions(await api.listSessions());
    } catch (reason) {
      setError(errorMessage(reason, "Unable to list sessions."));
    } finally {
      setLoading(false);
    }
  }, [api]);
  useEffect(() => { void reload(); }, [reload]);

  return <main className="machineemu-page">
    <Link className="back-link" to="/">← Catalog</Link>
    <Header />
    <section className="panel" aria-labelledby="sessions-title">
      <h2 id="sessions-title">Sessions</h2>
      <p className="session-meta">Only complete, MachineEmu-owned session records are shown.</p>
      <div className="actions"><button className="button button-secondary" disabled={loading} onClick={() => void reload()}>Refresh</button></div>
      {error ? <ErrorNotice>{error}</ErrorNotice> : null}
      {!loading && !error && !sessions.length ? <p className="notice">No sessions found.</p> : null}
      <div className="session-list">
        {sessions.map((session) => <Link className="session-row" key={session.session_id}
          to={`/sessions/${encodeURIComponent(session.session_id)}?instance=${encodeURIComponent(session.instance_id)}`}>
          <strong>{session.session_id}</strong>
          <span>{session.machine} · {session.state}</span>
          <span>{session.instance_id} · {Object.entries(session.capabilities ?? {}).filter(([, capability]) => capability.available).map(([name]) => name).join(", ") || "no declared views"}</span>
        </Link>)}
      </div>
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
  const [qmp, setQmp] = useState<string>();
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
      void api.qmpStatus(instance, id).then((value) => setQmp(value.status)).catch(() => setQmp(undefined));
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
        {qmp && <p className="session-meta">QMP: {qmp}</p>}
        <div className="actions">
          <Link className="button button-secondary" to={`/sessions/${encodeURIComponent(id)}/terminal?instance=${encodeURIComponent(instance)}`}>Terminal</Link>
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

function terminalUrl(instance: string, session: string, ticket: string): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws/v1/sessions/${encodeURIComponent(instance)}/${encodeURIComponent(session)}/terminal?ticket=${encodeURIComponent(ticket)}`;
}

function Terminal() {
  const api = useMemo(client, []);
  const { id = "" } = useParams();
  const [query] = useSearchParams();
  const instance = query.get("instance") ?? "";
  const socket = useRef<WebSocket | null>(null);
  const decoder = useRef(new TextDecoder());
  const [output, setOutput] = useState("");
  const [input, setInput] = useState("");
  const [connection, setConnection] = useState("Connecting…");
  const [claimed, setClaimed] = useState(false);
  const [error, setError] = useState<string>();
  const validRoute = Boolean(instance && id);

  const connect = useCallback(async () => {
    if (!validRoute) return;
    socket.current?.close();
    setConnection("Requesting terminal access…"); setClaimed(false); setError(undefined);
    try {
      const ticket = await api.createTerminalTicket(instance, id);
      const next = new WebSocket(terminalUrl(instance, id, ticket.ticket));
      next.binaryType = "arraybuffer";
      socket.current = next;
      next.onopen = () => setConnection("Connected · view only");
      next.onmessage = (event) => {
        if (typeof event.data === "string") {
          try {
            const message = JSON.parse(event.data) as { type?: string };
            if (message.type === "terminal.claimed") { setClaimed(true); setConnection("Connected · control enabled"); }
            if (message.type === "terminal.released") { setClaimed(false); setConnection("Connected · view only"); }
          } catch { setError("Terminal control response was invalid."); }
          return;
        }
        if (event.data instanceof ArrayBuffer) {
          const text = decoder.current.decode(event.data, { stream: true });
          setOutput((current) => (current + text).slice(-200_000));
        }
      };
      next.onerror = () => setError("Terminal connection failed.");
      next.onclose = () => { setClaimed(false); setConnection("Disconnected"); };
    } catch (reason) { setError(errorMessage(reason, "Unable to open the terminal.")); setConnection("Unavailable"); }
  }, [api, id, instance, validRoute]);

  useEffect(() => { void connect(); return () => socket.current?.close(); }, [connect]);
  function control(type: "terminal.claim" | "terminal.release") {
    if (socket.current?.readyState === WebSocket.OPEN) socket.current.send(JSON.stringify({ v: 1, type }));
  }
  function send(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!claimed || !input || socket.current?.readyState !== WebSocket.OPEN) return;
    socket.current.send(new TextEncoder().encode(`${input}\n`));
    setInput("");
  }

  return <main className="machineemu-page">
    <Link className="back-link" to={`/sessions/${encodeURIComponent(id)}?instance=${encodeURIComponent(instance)}`}>← Session</Link><Header />
    <section className="panel" aria-labelledby="terminal-title"><h2 id="terminal-title">UART terminal</h2>
      {!validRoute ? <ErrorNotice>The terminal URL must include an instance ID.</ErrorNotice> : <>
        <p className="session-meta" role="status">{connection}</p>
        <div className="actions">
          {claimed ? <button className="button button-secondary" onClick={() => control("terminal.release")}>Release control</button>
            : <button className="button" disabled={connection !== "Connected · view only"} onClick={() => control("terminal.claim")}>Take control</button>}
          <button className="button button-secondary" onClick={() => void connect()}>Reconnect</button>
        </div>
        {error && <ErrorNotice>{error}</ErrorNotice>}
        <pre className="terminal-output" aria-label="UART output">{output || "Waiting for UART output…"}</pre>
        <form className="terminal-input" onSubmit={send}><label>Send line<input value={input} disabled={!claimed} onChange={(event) => setInput(event.target.value)} /></label><button className="button" disabled={!claimed || !input}>Send</button></form>
        <p className="session-meta">Input remains disabled until you explicitly take control. Output is capped locally at 200 kB.</p>
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
    <Route path="/profiles/:id" element={<ProfileRoute />} />
    <Route path="/sessions" element={<Sessions />} />
    <Route path="/sessions/:id/terminal" element={<Terminal />} />
    <Route path="/sessions/:id" element={<Session />} />
    <Route path="*" element={<NotFound />} />
  </Routes>;
}
