import { useEffect, useRef, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { MachineEmuClient } from "./client";

const HEADER = 16;
const MAX_RECORD = 16 * 1024 * 1024;

function socketUrl(instance: string, session: string, clientId: string): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws/v1/sessions/${encodeURIComponent(instance)}/${encodeURIComponent(session)}/video?client_id=${encodeURIComponent(clientId)}`;
}

function client(): MachineEmuClient {
  return new MachineEmuClient({ token: "" });
}

function records(buffer: Uint8Array<ArrayBuffer>): [Uint8Array<ArrayBuffer>[], Uint8Array<ArrayBuffer>] {
  const result: Uint8Array<ArrayBuffer>[] = [];
  let offset = 0;
  while (buffer.byteLength - offset >= HEADER) {
    const view = new DataView(buffer.buffer, buffer.byteOffset + offset, HEADER);
    const length = view.getUint32(4);
    if (length > MAX_RECORD || view.getUint16(2) !== 0) throw new Error("Invalid video record");
    if (buffer.byteLength - offset < HEADER + length) break;
    result.push(buffer.slice(offset, offset + HEADER + length));
    offset += HEADER + length;
  }
  return [result, buffer.slice(offset)];
}

function annexBToAvc(payload: Uint8Array<ArrayBuffer>): Uint8Array<ArrayBuffer> {
  const starts: Array<[number, number]> = [];
  for (let index = 0; index + 3 <= payload.byteLength;) {
    const four = index + 4 <= payload.byteLength && payload[index] === 0 && payload[index + 1] === 0 && payload[index + 2] === 0 && payload[index + 3] === 1;
    const three = payload[index] === 0 && payload[index + 1] === 0 && payload[index + 2] === 1;
    if (four || three) { starts.push([index, four ? 4 : 3]); index += four ? 4 : 3; } else index += 1;
  }
  const chunks = starts.map(([start, prefix], index) => payload.slice(start + prefix, index + 1 < starts.length ? starts[index + 1][0] : payload.byteLength)).filter((chunk) => chunk.byteLength);
  const total = chunks.reduce((size, chunk) => size + 4 + chunk.byteLength, 0);
  const output = new Uint8Array(total);
  const view = new DataView(output.buffer);
  let offset = 0;
  for (const chunk of chunks) { view.setUint32(offset, chunk.byteLength); offset += 4; output.set(chunk, offset); offset += chunk.byteLength; }
  return output;
}

export function Video() {
  const { id = "" } = useParams();
  const [query] = useSearchParams();
  const instance = query.get("instance") ?? "";
  const canvas = useRef<HTMLCanvasElement>(null);
  const socket = useRef<WebSocket | undefined>(undefined);
  const [clientId] = useState(() => crypto.randomUUID().replaceAll("-", ""));
  const [connected, setConnected] = useState(false);
  const [inputOwned, setInputOwned] = useState(false);
  const [error, setError] = useState<string>();

  useEffect(() => {
    if (!instance || !id || typeof VideoDecoder === "undefined") {
      if (instance && id) setError("This browser does not support WebCodecs.");
      return;
    }
    let stopped = false;
    let decoder: VideoDecoder | undefined;
    let configured = false;
    let keyframeNeeded = true;
    let buffer: Uint8Array<ArrayBuffer> = new Uint8Array(new ArrayBuffer(0));
    const next = new WebSocket(socketUrl(instance, id, clientId));
    next.binaryType = "arraybuffer";
    socket.current = next;
    const fail = (reason: unknown) => { if (!stopped) setError(reason instanceof Error ? reason.message : "Video stream failed."); };
    const createDecoder = () => {
      decoder = new VideoDecoder({
        output: (frame) => {
          const target = canvas.current;
          if (target) { target.width = frame.displayWidth; target.height = frame.displayHeight; target.getContext("2d")?.drawImage(frame, 0, 0); }
          frame.close();
        },
        error: (reason) => { configured = false; keyframeNeeded = true; fail(reason); },
      });
    };
    createDecoder();
    next.onopen = () => { setConnected(true); next.send(JSON.stringify({ type: "request_idr" })); };
    next.onclose = () => { setConnected(false); setInputOwned(false); };
    next.onerror = () => fail("The H.264 display stream disconnected");
    next.onmessage = async (event) => {
      try {
        const incoming = new Uint8Array(event.data as ArrayBuffer);
        const merged = new Uint8Array(buffer.byteLength + incoming.byteLength); merged.set(buffer); merged.set(incoming, buffer.byteLength); buffer = merged;
        const [frames, remainder] = records(buffer); buffer = remainder;
        for (const record of frames) {
          const view = new DataView(record.buffer, record.byteOffset, record.byteLength);
          const type = view.getUint8(0); const payload = record.slice(HEADER);
          if (type === 0 || type === 3) {
            const raw = JSON.parse(new TextDecoder().decode(payload)) as VideoDecoderConfig & { description?: number[] };
            const { description, ...fields } = raw;
            const config: VideoDecoderConfig = { ...fields, ...(description ? { description: Uint8Array.from(description) } : {}) };
            if (!configured && !(await VideoDecoder.isConfigSupported(config)).supported) throw new Error("Unsupported H.264 configuration");
            decoder?.configure({ ...config, optimizeForLatency: true }); configured = true; keyframeNeeded = true;
          } else if ((type === 1 || type === 2) && configured && (!keyframeNeeded || type === 1)) {
            const converted = annexBToAvc(payload);
            const avc = new Uint8Array(new ArrayBuffer(converted.byteLength));
            avc.set(converted);
            decoder?.decode(new EncodedVideoChunk({ type: type === 1 ? "key" : "delta", timestamp: Number(view.getBigUint64(8)), data: avc }));
            if (type === 1) keyframeNeeded = false;
          }
        }
      } catch (reason) { fail(reason); next.close(); }
    };
    return () => { stopped = true; next.close(); if (decoder && decoder.state !== "closed") decoder.close(); };
  }, [clientId, id, instance]);

  async function claim(takeover = false) {
    try { await client().vncControl(instance, id, { action: "claim", client_id: clientId, takeover }); setInputOwned(true); }
    catch (reason) { setError(reason instanceof Error ? reason.message : "Unable to claim video input."); }
  }
  async function release() {
    try { await client().vncControl(instance, id, { action: "release", client_id: clientId }); setInputOwned(false); }
    catch (reason) { setError(reason instanceof Error ? reason.message : "Unable to release video input."); }
  }
  function send(type: string, fields: Record<string, unknown> = {}) {
    if (inputOwned && socket.current?.readyState === WebSocket.OPEN) socket.current.send(JSON.stringify({ type, ...fields }));
  }
  return <main className="machineemu-page">
    <Link className="back-link" to={`/sessions/${encodeURIComponent(id)}?instance=${encodeURIComponent(instance)}`}>← Session</Link>
    <header className="machineemu-header"><p className="eyebrow">LOCAL EMULATION</p><h1>H.264 display</h1></header>
    <p className="session-meta">{connected ? `Connected · ${inputOwned ? "input enabled" : "view only"}` : "Connecting…"}</p>
    <div className="actions"><button className="button" disabled={!connected} onClick={() => void claim()}>Take input</button><button className="button button-secondary" disabled={!connected} onClick={() => void claim(true)}>Take over</button><button className="button button-secondary" disabled={!inputOwned} onClick={() => void release()}>Release input</button></div>
    {error && <p className="notice notice-error" role="alert">{error}</p>}
    <canvas className="video-canvas" ref={canvas} aria-label="QEMU H.264 display" />
    <div className="actions"><button className="button button-secondary" onClick={() => socket.current?.send(JSON.stringify({ type: "request_idr" }))}>Refresh frame</button><button className="button button-secondary" disabled={!inputOwned} onClick={() => send("key_down", { key: "Escape" })}>Send Escape</button></div>
  </main>;
}
