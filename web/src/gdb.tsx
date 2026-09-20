import { FormEvent, useEffect, useRef, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";

type Frame = { type?: string; text?: string; stream?: string; error?: string; status?: string };

export function Gdb() {
  const { id = "" } = useParams();
  const [query] = useSearchParams();
  const instance = query.get("instance") ?? "";
  const [frames, setFrames] = useState<Frame[]>([]);
  const [command, setCommand] = useState("");
  const [ready, setReady] = useState(false);
  const socket = useRef<WebSocket | undefined>(undefined);
  useEffect(() => {
    if (!instance || !id) return;
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const next = new WebSocket(`${scheme}://${location.host}/ws/v1/sessions/${encodeURIComponent(instance)}/${encodeURIComponent(id)}/gdb?client_id=${crypto.randomUUID().replaceAll("-", "")}`);
    socket.current = next;
    next.onmessage = (event) => {
      try {
        const frame = JSON.parse(String(event.data)) as Frame;
        setFrames((current) => [...current, frame].slice(-500));
        if (frame.type === "gdb.ready") setReady(true);
      } catch { setFrames((current) => [...current, { type: "gdb.error", error: "Invalid GDB frame" }].slice(-500)); }
    };
    next.onclose = () => setReady(false);
    return () => { next.close(); socket.current = undefined; };
  }, [id, instance]);
  function submit(event: FormEvent) {
    event.preventDefault();
    if (!command.trim() || socket.current?.readyState !== WebSocket.OPEN) return;
    socket.current.send(JSON.stringify({ v: 1, type: "gdb.command", text: command }));
    setCommand("");
  }
  return <main className="machineemu-page">
    <Link className="back-link" to={`/sessions/${encodeURIComponent(id)}?instance=${encodeURIComponent(instance)}`}>← Session</Link>
    <header className="machineemu-header"><p className="eyebrow">LOCAL EMULATION</p><h1>GDB console</h1></header>
    {!instance || !id ? <p className="notice notice-error">The GDB URL must include an instance ID.</p> : <>
      <p className="session-meta" role="status">{ready ? "Connected" : "Connecting or unavailable"}</p>
      <pre className="gdb-output" aria-label="GDB transcript">{frames.map((frame, index) => `${frame.type ?? "frame"}: ${frame.text ?? frame.error ?? frame.status ?? ""}`).join("\n") || "Waiting for GDB…"}</pre>
      <form className="terminal-input" onSubmit={submit}><label>GDB command<input value={command} disabled={!ready} onChange={(event) => setCommand(event.target.value)} /></label><button className="button" disabled={!ready || !command.trim()}>Send</button></form>
    </>}
  </main>;
}
