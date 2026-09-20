/**
 * Browser-to-guest audio: the microphone into the record channel.
 *
 * Echo cancellation is asked for here rather than in the guest. The guest
 * hears a speaker signal that was routed separately and arrives late, so its
 * own canceller has nothing usable to cancel against; the browser has both
 * the captured and the played-back signal in front of it.
 */
import {
  AUDIO_MODE,
  MSGC_RECORD_DATA,
  MSGC_RECORD_MODE,
  MSGC_RECORD_START_MARK,
  MSG_RECORD_MUTE,
  MSG_RECORD_START,
  MSG_RECORD_STOP,
  MSG_RECORD_VOLUME,
  type AudioMode,
} from "./protocol";
import {
  encodeAudioData,
  encodeModeMessage,
  parseRecordStart,
  planesToS16,
  type SpiceMessage,
} from "./codec";
import { addWorklets } from "./worklets";

/** Capture frame length. 10 ms is what Opus prefers and what QEMU expects. */
const FRAME_MILLISECONDS = 10;

export type RecordState = {
  /** Whether the guest currently has its capture stream open. */
  guestListening: boolean;
  /** Whether the microphone is being sent. */
  sending: boolean;
  /** Frames sent since the stream opened. */
  frames: number;
};

/**
 * Feeds the microphone into one record channel.
 *
 * Nothing is captured until the guest opens its capture stream: the getUserMedia
 * prompt is raised when the guest asks for audio, not when the tab loads, and
 * the stream's tracks are stopped again as soon as the guest stops listening.
 */
export class SpiceRecord {
  private context: AudioContext | null = null;
  private stream: MediaStream | null = null;
  private node: AudioWorkletNode | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private encoder: AudioEncoder | null = null;
  private mode: AudioMode = AUDIO_MODE.raw;
  private channels = 2;
  private frequency = 48_000;
  private frameIndex = 0;
  private muted = false;
  state: RecordState = { guestListening: false, sending: false, frames: 0 };

  constructor(
    private readonly send: (kind: number, payload: Uint8Array) => void,
    private readonly serverTime: () => number,
    private readonly opus: boolean,
    private readonly onState: (state: RecordState) => void = () => {},
    private readonly onError: (error: Error) => void = () => {}
  ) {}

  /** Handle one record-channel message from the guest. */
  async handle(message: SpiceMessage): Promise<void> {
    if (message.kind === MSG_RECORD_START) {
      const start = parseRecordStart(message.payload);
      this.channels = start.channels;
      this.frequency = start.frequency;
      this.state = { ...this.state, guestListening: true };
      this.onState(this.state);
      await this.open();
      return;
    }
    if (message.kind === MSG_RECORD_STOP) {
      this.state = { ...this.state, guestListening: false };
      this.stop();
      return;
    }
    if (message.kind === MSG_RECORD_MUTE || message.kind === MSG_RECORD_VOLUME) {
      // Guest-side mute and volume are not applied to the captured signal:
      // the operator's own mute is the one that must be believed.
      return;
    }
  }

  /** Stop sending without closing the channel, as the mute button does. */
  setMuted(muted: boolean): void {
    this.muted = muted;
    for (const track of this.stream?.getAudioTracks() ?? []) track.enabled = !muted;
    this.state = { ...this.state, sending: this.state.guestListening && !muted };
    this.onState(this.state);
  }

  private async open(): Promise<void> {
    if (this.context) return;
    try {
      this.stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
          channelCount: this.channels,
          sampleRate: this.frequency,
        },
      });
    } catch (error) {
      this.onError(
        error instanceof Error && error.name === "NotAllowedError"
          ? new Error("Microphone access was denied, so the guest will hear silence")
          : new Error("The microphone could not be opened")
      );
      return;
    }
    // Ask for the guest's rate directly: resampling here would add latency
    // and the browser can usually oblige.
    this.context = new AudioContext({ sampleRate: this.frequency });
    await addWorklets(this.context);
    const frameSize = Math.round((this.frequency * FRAME_MILLISECONDS) / 1000);
    this.node = new AudioWorkletNode(this.context, "spice-record", {
      numberOfOutputs: 0,
      processorOptions: { channels: this.channels, frameSize },
    });
    this.node.port.onmessage = (event) =>
      this.frame((event.data as { planes: Float32Array[] }).planes);
    this.source = this.context.createMediaStreamSource(this.stream);
    this.source.connect(this.node);

    this.mode = this.opus ? AUDIO_MODE.opus : AUDIO_MODE.raw;
    if (this.mode === AUDIO_MODE.opus) this.openEncoder();
    this.send(MSGC_RECORD_MODE, encodeModeMessage(this.serverTime(), this.mode));
    this.send(MSGC_RECORD_START_MARK, timestamp(this.serverTime()));
    this.frameIndex = 0;
    this.setMuted(this.muted);
  }

  private openEncoder(): void {
    if (typeof AudioEncoder === "undefined") {
      // Fall back rather than fail: raw S16 is wasteful across a remote
      // ingress but correct, and QEMU accepts it on every build.
      this.mode = AUDIO_MODE.raw;
      return;
    }
    this.encoder = new AudioEncoder({
      output: (chunk) => {
        const samples = new Uint8Array(chunk.byteLength);
        chunk.copyTo(samples);
        this.emit(samples, Math.round(chunk.timestamp / 1000));
      },
      error: () => {
        this.encoder = null;
        this.mode = AUDIO_MODE.raw;
        this.send(MSGC_RECORD_MODE, encodeModeMessage(this.serverTime(), this.mode));
      },
    });
    this.encoder.configure({
      codec: "opus",
      sampleRate: this.frequency,
      numberOfChannels: this.channels,
    });
  }

  private frame(planes: Float32Array[]): void {
    if (this.muted || !this.state.guestListening) return;
    const time = this.serverTime();
    if (this.mode === AUDIO_MODE.opus && this.encoder) {
      const interleaved = new Float32Array(planes[0].length * planes.length);
      for (let frame = 0; frame < planes[0].length; frame += 1) {
        for (let channel = 0; channel < planes.length; channel += 1) {
          interleaved[frame * planes.length + channel] = planes[channel][frame];
        }
      }
      this.encoder.encode(
        new AudioData({
          format: "f32",
          sampleRate: this.frequency,
          numberOfFrames: planes[0].length,
          numberOfChannels: planes.length,
          timestamp: time * 1000,
          data: interleaved,
        })
      );
      return;
    }
    this.emit(planesToS16(planes), time);
  }

  private emit(samples: Uint8Array, time: number): void {
    this.send(MSGC_RECORD_DATA, encodeAudioData(time, samples));
    this.frameIndex += 1;
    if (this.frameIndex % 100 === 0) {
      this.state = { ...this.state, frames: this.frameIndex };
      this.onState(this.state);
    }
  }

  /** Release the microphone and tear down the capture graph. */
  stop(): void {
    try {
      this.encoder?.close();
    } catch {
      /* Already closed. */
    }
    this.encoder = null;
    this.node?.port.postMessage({ type: "stop" });
    this.source?.disconnect();
    this.node?.disconnect();
    for (const track of this.stream?.getTracks() ?? []) track.stop();
    void this.context?.close();
    this.context = null;
    this.stream = null;
    this.node = null;
    this.source = null;
    this.state = { ...this.state, sending: false };
    this.onState(this.state);
  }
}

function timestamp(value: number): Uint8Array {
  const out = new Uint8Array(4);
  new DataView(out.buffer).setUint32(0, value >>> 0, true);
  return out;
}
