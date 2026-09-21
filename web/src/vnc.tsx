import { useEffect, useRef, useState } from "react";
import RFB from "@novnc/novnc";
import { Link, useParams, useSearchParams } from "react-router";
import { MachineEmuClient } from "./client";
import { VncAudio } from "./vnc-audio";

function socketUrl(instance: string, session: string, clientId: string): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws/v1/sessions/${encodeURIComponent(instance)}/${encodeURIComponent(session)}/vnc?client_id=${encodeURIComponent(clientId)}`;
}

function client(): MachineEmuClient {
  return new MachineEmuClient({ token: "" });
}

export function Vnc() {
  const { id = "" } = useParams();
  const [query] = useSearchParams();
  const instance = query.get("instance") ?? "";
  const host = useRef<HTMLDivElement>(null);
  const rfb = useRef<RFB | undefined>(undefined);
  const [clientId] = useState(() => crypto.randomUUID().replaceAll("-", ""));
  const [connected, setConnected] = useState(false);
  const [viewOnly, setViewOnly] = useState(true);
  const [error, setError] = useState<string>();

  useEffect(() => {
    if (!instance || !id || !host.current) return;
    const viewer = new RFB(host.current, socketUrl(instance, id, clientId));
    viewer.viewOnly = true;
    viewer.scaleViewport = true;
    viewer.clipViewport = false;
    viewer.resizeSession = false;
    rfb.current = viewer;
    const onConnect = () => {
      setConnected(true);
      setError(undefined);
    };
    const onDisconnect = () => {
      setConnected(false);
      setViewOnly(true);
    };
    viewer.addEventListener("connect", onConnect);
    viewer.addEventListener("disconnect", onDisconnect);
    return () => {
      viewer.removeEventListener("connect", onConnect);
      viewer.removeEventListener("disconnect", onDisconnect);
      viewer.disconnect();
      rfb.current = undefined;
    };
  }, [clientId, id, instance]);

  async function claim(takeover = false) {
    if (!connected) return;
    try {
      await client().vncControl(instance, id, { action: "claim", client_id: clientId, takeover });
      if (rfb.current) rfb.current.viewOnly = false;
      setViewOnly(false);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Unable to claim VNC input.");
    }
  }

  async function release() {
    try {
      await client().vncControl(instance, id, { action: "release", client_id: clientId });
      if (rfb.current) rfb.current.viewOnly = true;
      setViewOnly(true);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Unable to release VNC input.");
    }
  }

  return <main className="machineemu-page">
    <Link className="back-link" to={`/sessions/${encodeURIComponent(id)}?instance=${encodeURIComponent(instance)}`}>← Session</Link>
    <Header title="VNC display" />
    {!instance || !id ? <ErrorNotice>The VNC URL must include an instance ID.</ErrorNotice> : <>
      <p className="session-meta" role="status">{connected ? `Connected · ${viewOnly ? "view only" : "input enabled"}` : "Connecting…"}</p>
      <div className="actions">
        {viewOnly ? <><button className="button" disabled={!connected} onClick={() => void claim()}>Take input</button>
          <button className="button button-secondary" disabled={!connected} onClick={() => void claim(true)}>Take over</button></>
          : <button className="button button-secondary" onClick={() => void release()}>Release input</button>}
        <button className="button button-secondary" disabled={viewOnly} onClick={() => rfb.current?.sendCtrlAltDel()}>Ctrl+Alt+Delete</button>
        <VncAudio instanceId={instance} sessionId={id} />
      </div>
      {error && <ErrorNotice>{error}</ErrorNotice>}
      <div className="vnc-host" ref={host} aria-label="QEMU VNC display" />
    </>}
  </main>;
}

function Header({ title }: { title: string }) {
  return <header className="machineemu-header"><p className="eyebrow">LOCAL EMULATION</p><h1>{title}</h1></header>;
}

function ErrorNotice({ children }: { children: string }) {
  return <p className="notice notice-error" role="alert">{children}</p>;
}
