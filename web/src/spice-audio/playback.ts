/**
 * Guest-to-browser audio: the playback channel into the speakers.
 *
 * Opus frames go through the WebCodecs `AudioDecoder` and raw S16 is
 * converted directly; both end up as float planes posted into an
 * AudioWorklet that owns the jitter buffer. spice-html5 wraps Opus in WebM
 * and feeds MediaSource, which adds a container and several hundred
 * milliseconds of buffering to a path that is meant to be conversational.
 */
import {
  AUDIO_MODE,
  MSG_PLAYBACK_DATA,
  MSG_PLAYBACK_MODE,
  MSG_PLAYBACK_MUTE,
  MSG_PLAYBACK_START,
  MSG_PLAYBACK_STOP,
  MSG_PLAYBACK_VOLUME,
  type AudioMode,
} from "./protocol";
import {
  parseAudioData,
  parseModeMessage,
  parsePlaybackStart,
  s16ToPlanes,
  type PlaybackStart,
  type SpiceMessage,
} from "./codec";
import { addWorklets } from "./worklets";

/** How much decoded audio the worklet may hold before it drops the oldest. */
const MAX_BUFFERED_SECONDS = 0.4;

export type PlaybackStats = {
  /** Frames waiting in the worklet's queue. */
  queued: number;
  /** Frames of silence produced because the queue was empty. */
  underruns: number;
  /** Frames discarded because the queue overran. */
  dropped: number;
};

/**
 * Sinks playback messages into an `AudioContext`.
 *
 * The context is supplied by the caller because it must be created, or
 * resumed, inside a user gesture: browsers will not start audio otherwise.
 */
export class SpicePlayback {
  private node: AudioWorkletNode | null = null;
  private decoder: AudioDecoder | null = null;
  private mode: AudioMode = AUDIO_MODE.raw;
  private format: PlaybackStart | null = null;
  private gain: GainNode | null = null;
  private muted = false;
  stats: PlaybackStats = { queued: 0, underruns: 0, dropped: 0 };

  constructor(
    private readonly context: AudioContext,
    private readonly onStats: (stats: PlaybackStats) => void = () => {}
  ) {}

  /** Handle one playback-channel message. */
  async handle(message: SpiceMessage): Promise<void> {
    switch (message.kind) {
      case MSG_PLAYBACK_MODE:
        this.mode = parseModeMessage(message.payload).mode;
        return;
      case MSG_PLAYBACK_START:
        await this.start(parsePlaybackStart(message.payload));
        return;
      case MSG_PLAYBACK_DATA:
        this.data(parseAudioData(message.payload));
        return;
      case MSG_PLAYBACK_STOP:
        this.node?.port.postMessage({ type: "reset" });
        return;
      case MSG_PLAYBACK_MUTE:
        // The guest's mute is advisory here: the speaker control in the
        // toolbar is the one the operator sees, so the guest's request only
        // takes effect while the operator has not muted as well.
        this.muted = message.payload.length > 0 && message.payload[0] !== 0;
        this.applyGain();
        return;
      case MSG_PLAYBACK_VOLUME:
      default:
        return;
    }
  }

  /** Silence or restore the speakers without disturbing the stream. */
  setMuted(muted: boolean): void {
    this.muted = muted;
    this.applyGain();
  }

  private applyGain(): void {
    if (this.gain) this.gain.gain.value = this.muted ? 0 : 1;
  }

  private async start(format: PlaybackStart): Promise<void> {
    const changed =
      this.format === null ||
      this.format.channels !== format.channels ||
      this.format.frequency !== format.frequency;
    this.format = format;
    if (this.node && !changed) {
      this.node.port.postMessage({ type: "reset" });
      return;
    }
    this.teardown();
    await addWorklets(this.context);
    this.node = new AudioWorkletNode(this.context, "spice-playback", {
      numberOfInputs: 0,
      outputChannelCount: [format.channels],
      processorOptions: { maxFrames: Math.round(format.frequency * MAX_BUFFERED_SECONDS) },
    });
    this.node.port.onmessage = (event) => {
      this.stats = event.data as PlaybackStats;
      this.onStats(this.stats);
    };
    this.gain = this.context.createGain();
    this.applyGain();
    this.node.connect(this.gain).connect(this.context.destination);
    if (this.mode === AUDIO_MODE.opus) this.openDecoder(format);
  }

  private openDecoder(format: PlaybackStart): void {
    if (typeof AudioDecoder === "undefined") {
      throw new Error("this browser has no WebCodecs AudioDecoder for Opus playback");
    }
    this.decoder = new AudioDecoder({
      output: (frame) => {
        this.push(this.planesFrom(frame));
        frame.close();
      },
      error: () => {
        // A decoder that has errored cannot be reused; drop it and let the
        // next MSG_PLAYBACK_START build a fresh one.
        this.decoder = null;
      },
    });
    this.decoder.configure({
      codec: "opus",
      sampleRate: format.frequency,
      numberOfChannels: format.channels,
    });
  }

  private planesFrom(frame: AudioData): Float32Array[] {
    const planes: Float32Array[] = [];
    for (let channel = 0; channel < frame.numberOfChannels; channel += 1) {
      const plane = new Float32Array(frame.numberOfFrames);
      frame.copyTo(plane, { planeIndex: channel, format: "f32-planar" });
      planes.push(plane);
    }
    return planes;
  }

  private data(data: { time: number; samples: Uint8Array }): void {
    if (!this.node || this.format === null || data.samples.length === 0) return;
    if (this.mode === AUDIO_MODE.opus) {
      if (this.decoder === null) this.openDecoder(this.format);
      this.decoder?.decode(
        new EncodedAudioChunk({
          type: "key",
          // SPICE timestamps are milliseconds on the server's media clock;
          // WebCodecs wants microseconds.
          timestamp: data.time * 1000,
          data: data.samples,
        })
      );
      return;
    }
    if (this.mode === AUDIO_MODE.celt) return; // never negotiated
    this.push(s16ToPlanes(data.samples, this.format.channels));
  }

  private push(planes: Float32Array[]): void {
    if (planes.length === 0 || planes[0].length === 0) return;
    this.node?.port.postMessage(
      { planes },
      planes.map((plane) => plane.buffer)
    );
  }

  /** Disconnect the graph and release the decoder. */
  teardown(): void {
    try {
      this.decoder?.close();
    } catch {
      /* Already closed, or never configured. */
    }
    this.decoder = null;
    this.node?.disconnect();
    this.gain?.disconnect();
    this.node = null;
    this.gain = null;
  }
}
