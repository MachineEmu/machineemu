import { useEffect, useRef, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { MachineEmuClient } from "./client";

const HEADER = 16;
const MAX_RECORD = 16 * 1024 * 1024;

type CursorRecord = { type: "cursor"; x: number; y: number; visible: boolean; width: number; height: number; hot_x: number; hot_y: number; data: number[] };

function validCursor(value: CursorRecord): boolean {
  return value.type === "cursor" && Number.isInteger(value.width) && Number.isInteger(value.height)
    && value.width >= 0 && value.height >= 0 && value.width <= 256 && value.height <= 256
    && Number.isInteger(value.hot_x) && Number.isInteger(value.hot_y)
    && value.hot_x >= 0 && value.hot_y >= 0
    && (value.width === 0 || value.hot_x < value.width)
    && (value.height === 0 || value.hot_y < value.height)
    && Array.isArray(value.data) && value.data.length === value.width * value.height * 4;
}

function socketUrl(instance: string, ticket: string): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws/v2/instances/${encodeURIComponent(instance)}/video?ticket=${encodeURIComponent(ticket)}`;
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
  const cursorCanvas = useRef<HTMLCanvasElement>(null);
  const socket = useRef<WebSocket | undefined>(undefined);
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
    let cursor: CursorRecord | undefined;
    const drawCursor = () => {
      const overlay = cursorCanvas.current;
      if (!overlay) return;
      const context = overlay.getContext("2d");
      if (!context) return;
      context.clearRect(0, 0, overlay.width, overlay.height);
      if (!cursor?.visible || !validCursor(cursor) || !cursor.width || !cursor.height) return;
      const pixels = new Uint8ClampedArray(cursor.data);
      for (let offset = 0; offset < pixels.length; offset += 4) {
        [pixels[offset], pixels[offset + 2]] = [pixels[offset + 2], pixels[offset]];
      }
      const bitmap = new ImageData(pixels, cursor.width, cursor.height);
      const shape = document.createElement("canvas");
      shape.width = cursor.width; shape.height = cursor.height;
      shape.getContext("2d")?.putImageData(bitmap, 0, 0);
      context.drawImage(shape, cursor.x - cursor.hot_x, cursor.y - cursor.hot_y);
    };
    const fail = (reason: unknown) => { if (!stopped) setError(reason instanceof Error ? reason.message : "Video stream failed."); };
    const createDecoder = () => {
      decoder = new VideoDecoder({
        output: (frame) => {
          const target = canvas.current;
          if (target) {
            target.width = frame.displayWidth; target.height = frame.displayHeight;
            target.getContext("2d")?.drawImage(frame, 0, 0);
            const overlay = cursorCanvas.current;
            if (overlay && (overlay.width !== frame.displayWidth || overlay.height !== frame.displayHeight)) {
              overlay.width = frame.displayWidth; overlay.height = frame.displayHeight;
              drawCursor();
            }
          }
          frame.close();
        },
        error: (reason) => { configured = false; keyframeNeeded = true; fail(reason); },
      });
    };
    createDecoder();
    let next: WebSocket | undefined;
    void client().streamTicket(instance, "video", { control: true }).then(({ ticket }) => {
      if (stopped) return;
      next = new WebSocket(socketUrl(instance, ticket)); next.binaryType = "arraybuffer"; socket.current = next;
      next.onopen = () => { setConnected(true); setInputOwned(true); next?.send(JSON.stringify({ type: "request_idr" })); };
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
          } else if (type === 4) {
            const value = JSON.parse(new TextDecoder().decode(payload)) as CursorRecord;
            if (value.type === "cursor" && validCursor(value)) { cursor = value; drawCursor(); }
          }
        }
      } catch (reason) { fail(reason); next?.close(); }
      };
    }).catch(fail);
    return () => { stopped = true; next?.close(); if (decoder && decoder.state !== "closed") decoder.close(); };
  }, [id, instance]);
  function send(type: string, fields: Record<string, unknown> = {}) {
    if (inputOwned && socket.current?.readyState === WebSocket.OPEN) socket.current.send(JSON.stringify({ type, ...fields }));
  }
  return <main className="machineemu-page">
    <Link className="back-link" to={`/sessions/${encodeURIComponent(id)}?instance=${encodeURIComponent(instance)}`}>← Session</Link>
    <header className="machineemu-header"><p className="eyebrow">LOCAL EMULATION</p><h1>H.264 display</h1></header>
    <p className="session-meta">{connected ? `Connected · ${inputOwned ? "input enabled" : "view only"}` : "Connecting…"}</p>
    <div className="actions"><span className="session-meta">{inputOwned ? "Input enabled" : "View only"}</span></div>
    {error && <p className="notice notice-error" role="alert">{error}</p>}
    <div className="video-canvas-wrap"><canvas className="video-canvas" ref={canvas} aria-label="QEMU H.264 display" /><canvas className="video-cursor-canvas" ref={cursorCanvas} aria-hidden="true" /></div>
    <div className="actions"><button className="button button-secondary" onClick={() => socket.current?.send(JSON.stringify({ type: "request_idr" }))}>Refresh frame</button><button className="button button-secondary" disabled={!inputOwned} onClick={() => send("key_down", { key: "Escape" })}>Send Escape</button></div>
  </main>;
}
