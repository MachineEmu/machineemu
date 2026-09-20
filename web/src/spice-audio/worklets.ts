/**
 * AudioWorklet processors, compiled from source strings at runtime.
 *
 * They are built into blob URLs rather than shipped as separate modules so
 * the bundler needs no worklet-specific configuration, and they pass audio
 * through `postMessage` rather than a `SharedArrayBuffer`: sharing memory
 * with a worklet needs COOP and COEP headers on the whole application, which
 * would put the cross-origin noVNC embedding at risk for no audible gain.
 */

const PLAYBACK_SOURCE = `
/**
 * Queue decoded frames and hand them to the audio device on demand.
 *
 * The queue is bounded: when the guest produces faster than the device
 * consumes, the oldest frames are dropped rather than accumulating latency
 * that never drains. Underrun is silence, which is the right failure for a
 * conference call.
 */
class SpicePlaybackProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.queue = [];
    this.offset = 0;
    this.queued = 0;
    this.maxFrames = options.processorOptions.maxFrames;
    this.underruns = 0;
    this.dropped = 0;
    this.reported = 0;
    this.port.onmessage = (event) => {
      const message = event.data;
      if (message.type === "reset") {
        this.queue = [];
        this.offset = 0;
        this.queued = 0;
        return;
      }
      this.queue.push(message.planes);
      this.queued += message.planes[0].length;
      while (this.queued > this.maxFrames && this.queue.length > 1) {
        const dropped = this.queue.shift();
        this.queued -= dropped[0].length - this.offset;
        this.dropped += dropped[0].length - this.offset;
        this.offset = 0;
      }
    };
  }

  process(_inputs, outputs) {
    const output = outputs[0];
    const frames = output[0].length;
    let written = 0;
    while (written < frames && this.queue.length > 0) {
      const planes = this.queue[0];
      const available = planes[0].length - this.offset;
      const take = Math.min(available, frames - written);
      for (let channel = 0; channel < output.length; channel += 1) {
        const plane = planes[Math.min(channel, planes.length - 1)];
        output[channel].set(plane.subarray(this.offset, this.offset + take), written);
      }
      this.offset += take;
      this.queued -= take;
      written += take;
      if (this.offset >= planes[0].length) {
        this.queue.shift();
        this.offset = 0;
      }
    }
    if (written < frames) {
      this.underruns += frames - written;
      for (const channel of output) channel.fill(0, written);
    }
    this.reported += frames;
    if (this.reported >= sampleRate) {
      this.reported = 0;
      this.port.postMessage({ queued: this.queued, underruns: this.underruns, dropped: this.dropped });
      this.underruns = 0;
      this.dropped = 0;
    }
    return true;
  }
}

registerProcessor("spice-playback", SpicePlaybackProcessor);
`;

const RECORD_SOURCE = `
/**
 * Slice the microphone into fixed frames and post them to the main thread.
 *
 * The guest expects a steady cadence, and a render quantum (128 frames) is
 * too small to be worth one SPICE message each, so quanta are gathered into
 * frames of the size the client asked for.
 */
class SpiceRecordProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.channels = options.processorOptions.channels;
    this.frameSize = options.processorOptions.frameSize;
    this.planes = this.allocate();
    this.filled = 0;
    this.active = true;
    this.port.onmessage = (event) => {
      if (event.data.type === "stop") this.active = false;
    };
  }

  allocate() {
    const planes = [];
    for (let channel = 0; channel < this.channels; channel += 1) {
      planes.push(new Float32Array(this.frameSize));
    }
    return planes;
  }

  process(inputs) {
    if (!this.active) return false;
    const input = inputs[0];
    if (!input || input.length === 0) return true;
    const frames = input[0].length;
    let read = 0;
    while (read < frames) {
      const take = Math.min(frames - read, this.frameSize - this.filled);
      for (let channel = 0; channel < this.channels; channel += 1) {
        const source = input[Math.min(channel, input.length - 1)];
        this.planes[channel].set(source.subarray(read, read + take), this.filled);
      }
      this.filled += take;
      read += take;
      if (this.filled >= this.frameSize) {
        const planes = this.planes;
        this.planes = this.allocate();
        this.filled = 0;
        this.port.postMessage({ planes }, planes.map((plane) => plane.buffer));
      }
    }
    return true;
  }
}

registerProcessor("spice-record", SpiceRecordProcessor);
`;

const urls = new Map<string, string>();

function blobUrl(name: string, source: string): string {
  let url = urls.get(name);
  if (url === undefined) {
    url = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
    urls.set(name, url);
  }
  return url;
}

/** Register both processors on `context`, once per context. */
export async function addWorklets(context: AudioContext): Promise<void> {
  await context.audioWorklet.addModule(blobUrl("playback", PLAYBACK_SOURCE));
  await context.audioWorklet.addModule(blobUrl("record", RECORD_SOURCE));
}
