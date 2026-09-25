import { useEffect, useRef, useState } from "react";
import RFB from "@novnc/novnc";
import { Link, useParams, useSearchParams } from "react-router";
import { MachineEmuClient } from "./client";

function socketUrl(instance: string, ticket: string): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws/v2/instances/${encodeURIComponent(instance)}/vnc?ticket=${encodeURIComponent(ticket)}`;
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
  const [connected, setConnected] = useState(false);
  const [viewOnly, setViewOnly] = useState(true);
  const [error, setError] = useState<string>();

  useEffect(() => {
    if (!instance || !id || !host.current) return;
    let viewer: RFB | undefined;
    let cancelled = false;
    void client().streamTicket(instance, "vnc", { control: true }).then(({ ticket }) => {
      if (cancelled || !host.current) return;
      viewer = new RFB(host.current, socketUrl(instance, ticket));
      viewer.viewOnly = false; viewer.scaleViewport = true; viewer.clipViewport = false; viewer.resizeSession = false; rfb.current = viewer;
      viewer.addEventListener("connect", () => { setConnected(true); setViewOnly(false); setError(undefined); });
      viewer.addEventListener("disconnect", () => { setConnected(false); setViewOnly(true); });
    }).catch((reason) => setError(reason instanceof Error ? reason.message : "Unable to issue a VNC ticket."));
    return () => { cancelled = true; viewer?.disconnect(); rfb.current = undefined; };
  }, [id, instance]);

  return <main className="machineemu-page">
    <Link className="back-link" to={`/sessions/${encodeURIComponent(id)}?instance=${encodeURIComponent(instance)}`}>← Session</Link>
    <Header title="VNC display" />
    {!instance || !id ? <ErrorNotice>The VNC URL must include an instance ID.</ErrorNotice> : <>
      <p className="session-meta" role="status">{connected ? `Connected · ${viewOnly ? "view only" : "input enabled"}` : "Connecting…"}</p>
      <div className="actions">
        <span className="session-meta">{viewOnly ? "View only" : "Input enabled"}</span>
        <button className="button button-secondary" disabled={viewOnly} onClick={() => rfb.current?.sendCtrlAltDel()}>Ctrl+Alt+Delete</button>
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
