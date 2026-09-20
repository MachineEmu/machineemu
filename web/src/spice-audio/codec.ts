/**
 * SPICE link and message framing for the browser.
 *
 * Every parser here is bounded: a length or capability offset that does not
 * lie inside the frame actually received is a `SpiceError`, not a short read.
 * The console proxy applies the same bounds on the way up, so a malformed
 * frame is refused twice rather than trusted once.
 */
import {
  AUDIO_FMT_S16,
  AUDIO_MODE,
  CHANNEL,
  COMMON_CAP_AUTH_SPICE,
  COMMON_CAP_MINI_HEADER,
  COMMON_CAP_PROTOCOL_AUTH_SELECTION,
  LINK_HEADER_BYTES,
  LINK_MESS_BYTES,
  LINK_REPLY_BYTES,
  MAGIC,
  MAX_LINK_BYTES,
  MAX_MESSAGE_BYTES,
  MINI_HEADER_BYTES,
  PLAYBACK_CAP_OPUS,
  RECORD_CAP_OPUS,
  TICKET_PUBKEY_BYTES,
  VERSION_MAJOR,
  VERSION_MINOR,
  type AudioMode,
  type ChannelName,
} from "./protocol";

export class SpiceError extends Error {}

/** A capability word with a single bit set, for capabilities below 32. */
export function capWord(bit: number): number {
  return (1 << (bit % 32)) >>> 0;
}

/** Whether capability `bit` is set in a capability word list. */
export function hasCap(words: number[], bit: number): boolean {
  const index = Math.floor(bit / 32);
  return index < words.length && (words[index] & (1 << (bit % 32))) !== 0;
}

function view(data: Uint8Array): DataView {
  return new DataView(data.buffer, data.byteOffset, data.byteLength);
}

function capabilities(body: Uint8Array, offset: number, count: number): number[] {
  if (count > MAX_LINK_BYTES / 4 || offset > MAX_LINK_BYTES) {
    throw new SpiceError("SPICE capability count exceeds the limit");
  }
  const end = offset + count * 4;
  if (end > body.length) throw new SpiceError("SPICE capabilities fall outside the link message");
  const words = view(body);
  return Array.from({ length: count }, (_unused, index) =>
    words.getUint32(offset + index * 4, true)
  );
}

/** Serialize a `SpiceLinkHeader` for a body of `size` bytes. */
export function encodeLinkHeader(size: number): Uint8Array {
  const out = new Uint8Array(LINK_HEADER_BYTES);
  const words = view(out);
  words.setUint32(0, MAGIC, true);
  words.setUint32(4, VERSION_MAJOR, true);
  words.setUint32(8, VERSION_MINOR, true);
  words.setUint32(12, size, true);
  return out;
}

/** Parse a `SpiceLinkHeader`, returning the body length that follows it. */
export function parseLinkHeader(frame: Uint8Array): { major: number; minor: number; size: number } {
  if (frame.length < LINK_HEADER_BYTES) throw new SpiceError("short SPICE link header");
  const words = view(frame);
  if (words.getUint32(0, true) !== MAGIC) throw new SpiceError("SPICE link magic is not REDQ");
  const major = words.getUint32(4, true);
  if (major !== VERSION_MAJOR) throw new SpiceError("unsupported SPICE major version");
  const size = words.getUint32(12, true);
  if (size < LINK_REPLY_BYTES || size > MAX_LINK_BYTES) {
    throw new SpiceError("SPICE link reply length is out of range");
  }
  return { major, minor: words.getUint32(8, true), size };
}

/**
 * Build the link message for one channel.
 *
 * The connection id is not the client's to choose: it is zero on the first
 * main link and thereafter the value the server sent in `MSG_MAIN_INIT`,
 * which the console proxy independently checks against the one it observed.
 */
export function encodeLinkMess(
  channel: ChannelName,
  connectionId: number,
  opus: boolean
): Uint8Array {
  const common =
    capWord(COMMON_CAP_PROTOCOL_AUTH_SELECTION) |
    capWord(COMMON_CAP_AUTH_SPICE) |
    capWord(COMMON_CAP_MINI_HEADER);
  const channelCaps: number[] = [];
  if (opus && channel === "playback") channelCaps.push(capWord(PLAYBACK_CAP_OPUS));
  if (opus && channel === "record") channelCaps.push(capWord(RECORD_CAP_OPUS));
  const out = new Uint8Array(LINK_MESS_BYTES + (1 + channelCaps.length) * 4);
  const words = view(out);
  words.setUint32(0, connectionId >>> 0, true);
  out[4] = CHANNEL[channel];
  out[5] = 0;
  words.setUint32(6, 1, true);
  words.setUint32(10, channelCaps.length, true);
  words.setUint32(14, LINK_MESS_BYTES, true);
  words.setUint32(LINK_MESS_BYTES, common >>> 0, true);
  channelCaps.forEach((cap, index) => words.setUint32(LINK_MESS_BYTES + 4 + index * 4, cap, true));
  return out;
}

export type LinkReply = {
  error: number;
  publicKey: Uint8Array;
  commonCaps: number[];
  channelCaps: number[];
};

/** Parse a `SpiceLinkReply` body, which excludes the preceding header. */
export function parseLinkReply(body: Uint8Array): LinkReply {
  if (body.length < LINK_REPLY_BYTES) throw new SpiceError("short SPICE link reply");
  if (body.length > MAX_LINK_BYTES) throw new SpiceError("SPICE link reply exceeds the size limit");
  const words = view(body);
  const numCommon = words.getUint32(166, true);
  const numChannel = words.getUint32(170, true);
  const offset = words.getUint32(174, true);
  return {
    error: words.getUint32(0, true),
    publicKey: body.slice(4, 4 + TICKET_PUBKEY_BYTES),
    commonCaps: capabilities(body, offset, numCommon),
    channelCaps: capabilities(body, offset + numCommon * 4, numChannel),
  };
}

export type SpiceMessage = { kind: number; payload: Uint8Array };

/** Serialize one message with its mini header. */
export function encodeMessage(kind: number, payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(MINI_HEADER_BYTES + payload.length);
  const words = view(out);
  words.setUint16(0, kind, true);
  words.setUint32(2, payload.length, true);
  out.set(payload, MINI_HEADER_BYTES);
  return out;
}

/**
 * Reassembles mini-header messages from a stream of arbitrary chunks.
 *
 * A WebSocket delivers whole frames, but nothing guarantees a frame holds a
 * whole SPICE message, so the framer carries the remainder between chunks
 * rather than assuming one frame is one message.
 */
export class Framer {
  private buffer = new Uint8Array(0);

  feed(chunk: Uint8Array): SpiceMessage[] {
    const grown = new Uint8Array(this.buffer.length + chunk.length);
    grown.set(this.buffer);
    grown.set(chunk, this.buffer.length);
    this.buffer = grown;
    const out: SpiceMessage[] = [];
    let offset = 0;
    while (this.buffer.length - offset >= MINI_HEADER_BYTES) {
      const words = view(this.buffer);
      const kind = words.getUint16(offset, true);
      const size = words.getUint32(offset + 2, true);
      if (size > MAX_MESSAGE_BYTES) throw new SpiceError("SPICE message exceeds the size limit");
      const end = offset + MINI_HEADER_BYTES + size;
      if (this.buffer.length < end) break;
      out.push({ kind, payload: this.buffer.slice(offset + MINI_HEADER_BYTES, end) });
      offset = end;
    }
    if (offset > 0) this.buffer = this.buffer.slice(offset);
    return out;
  }

  get pending(): number {
    return this.buffer.length;
  }

  clear(): void {
    this.buffer = new Uint8Array(0);
  }
}

/** `SpiceMsgMainInit`; only the connection id and the media clock are used. */
export function parseMainInit(payload: Uint8Array): {
  connectionId: number;
  multiMediaTime: number;
} {
  if (payload.length < 32) throw new SpiceError("MSG_MAIN_INIT is too short");
  const words = view(payload);
  return { connectionId: words.getUint32(0, true), multiMediaTime: words.getUint32(24, true) };
}

export type PlaybackStart = {
  channels: number;
  format: number;
  frequency: number;
  time: number;
};

/** `SpiceMsgPlaybackStart`: the format of the stream about to play. */
export function parsePlaybackStart(payload: Uint8Array): PlaybackStart {
  if (payload.length < 14) throw new SpiceError("MSG_PLAYBACK_START is too short");
  const words = view(payload);
  const format = words.getUint16(4, true);
  if (format !== AUDIO_FMT_S16) throw new SpiceError("unsupported SPICE sample format");
  return {
    channels: words.getUint32(0, true),
    format,
    frequency: words.getUint32(6, true),
    time: words.getUint32(10, true),
  };
}

/** `SpiceMsgRecordStart`: the format the guest opened for capture. */
export function parseRecordStart(payload: Uint8Array): {
  channels: number;
  format: number;
  frequency: number;
} {
  if (payload.length < 10) throw new SpiceError("MSG_RECORD_START is too short");
  const words = view(payload);
  const format = words.getUint16(4, true);
  if (format !== AUDIO_FMT_S16) throw new SpiceError("unsupported SPICE sample format");
  return {
    channels: words.getUint32(0, true),
    format,
    frequency: words.getUint32(6, true),
  };
}

/** A timestamped audio payload: `MSG_PLAYBACK_DATA` or `MSGC_RECORD_DATA`. */
export function parseAudioData(payload: Uint8Array): { time: number; samples: Uint8Array } {
  if (payload.length < 4) throw new SpiceError("audio data has no timestamp");
  return { time: view(payload).getUint32(0, true), samples: payload.slice(4) };
}

export function encodeAudioData(time: number, samples: Uint8Array): Uint8Array {
  const out = new Uint8Array(4 + samples.length);
  view(out).setUint32(0, time >>> 0, true);
  out.set(samples, 4);
  return out;
}

/** A coding-mode announcement: `MSG_PLAYBACK_MODE` or `MSGC_RECORD_MODE`. */
export function parseModeMessage(payload: Uint8Array): { time: number; mode: AudioMode } {
  if (payload.length < 6) throw new SpiceError("mode message is too short");
  const words = view(payload);
  const mode = words.getUint16(4, true);
  if (mode !== AUDIO_MODE.raw && mode !== AUDIO_MODE.celt && mode !== AUDIO_MODE.opus) {
    throw new SpiceError("unknown SPICE audio mode");
  }
  return { time: words.getUint32(0, true), mode: mode as AudioMode };
}

export function encodeModeMessage(time: number, mode: AudioMode): Uint8Array {
  const out = new Uint8Array(6);
  const words = view(out);
  words.setUint32(0, time >>> 0, true);
  words.setUint16(4, mode, true);
  return out;
}

/** Convert interleaved signed 16-bit samples into per-channel float planes. */
export function s16ToPlanes(samples: Uint8Array, channels: number): Float32Array[] {
  const frames = Math.floor(samples.length / 2 / channels);
  const words = view(samples);
  const planes = Array.from({ length: channels }, () => new Float32Array(frames));
  for (let frame = 0; frame < frames; frame += 1) {
    for (let channel = 0; channel < channels; channel += 1) {
      planes[channel][frame] = words.getInt16((frame * channels + channel) * 2, true) / 0x8000;
    }
  }
  return planes;
}

/** Convert per-channel float planes into interleaved signed 16-bit samples. */
export function planesToS16(planes: Float32Array[]): Uint8Array {
  const channels = planes.length;
  const frames = channels > 0 ? planes[0].length : 0;
  const out = new Uint8Array(frames * channels * 2);
  const words = view(out);
  for (let frame = 0; frame < frames; frame += 1) {
    for (let channel = 0; channel < channels; channel += 1) {
      const value = Math.max(-1, Math.min(1, planes[channel][frame]));
      words.setInt16((frame * channels + channel) * 2, Math.round(value * 0x7fff), true);
    }
  }
  return out;
}
