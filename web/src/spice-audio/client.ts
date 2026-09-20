/**
 * The browser's SPICE audio client.
 *
 * Main is linked first and hands back the connection id that playback and
 * record must carry. The console mints the client token that authorizes all
 * three; nothing here invents an identity. Losing main invalidates the token,
 * so a reconnect starts from a fresh attach rather than reattaching child
 * channels to a session the console has already forgotten.
 */
import { SpiceChannel } from "./channel";
import { parseMainInit, type SpiceMessage } from "./codec";
import { SpicePlayback, type PlaybackStats } from "./playback";
import { SpiceRecord, type RecordState } from "./record";
import { MSGC_MAIN_ATTACH_CHANNELS, MSG_MAIN_INIT, MSG_MAIN_MULTI_MEDIA_TIME } from "./protocol";

export type AudioStatus = {
  state: "idle" | "connecting" | "connected" | "failed";
  /** Whether this viewer holds the session's one capture lease. */
  capture: boolean;
  /** Whether the guest has its capture stream open. */
  guestListening: boolean;
  speakerMuted: boolean;
  microphoneMuted: boolean;
  error: string | null;
  stats: PlaybackStats | null;
};

type Attach = { client_token: string; expires_in: number; capture_ttl: number };

const IDLE: AudioStatus = {
  state: "idle",
  capture: false,
  guestListening: false,
  speakerMuted: false,
  microphoneMuted: false,
  error: null,
  stats: null,
};

/**
 * Drives one session's audio.
 *
 * `start` must be called from a user gesture: the `AudioContext` it creates
 * will not produce sound otherwise, and the microphone prompt needs one too.
 */
export class SpiceAudioClient {
  private token: string | null = null;
  private main: SpiceChannel | null = null;
  private playbackChannel: SpiceChannel | null = null;
  private recordChannel: SpiceChannel | null = null;
  private playback: SpicePlayback | null = null;
  private record: SpiceRecord | null = null;
  private context: AudioContext | null = null;
  private renewTimer: number | null = null;
  private captureTimer: number | null = null;
  private connectionId = 0;
  /** The server's media clock at `clockOrigin`, and the local time then. */
  private serverClock = 0;
  private clockOrigin = 0;
  private stopped = false;
  status: AudioStatus = IDLE;

  constructor(
    private readonly instanceId: string,
    private readonly sessionId: string,
    private readonly onStatus: (status: AudioStatus) => void
  ) {}

  private update(patch: Partial<AudioStatus>): void {
    this.status = { ...this.status, ...patch };
    this.onStatus(this.status);
  }

  private async control<T>(body: Record<string, unknown>): Promise<T> {
    const response = await fetch(`/api/v1/sessions/${encodeURIComponent(this.instanceId)}/${encodeURIComponent(this.sessionId)}/audio/control`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-MachineEmu-Token": document.querySelector<HTMLMetaElement>('meta[name="machineemu-token"]')?.content ?? "", "Idempotency-Key": crypto.randomUUID() },
      body: JSON.stringify(body),
    });
    if (!response.ok) {
      const detail = (await response.json().catch(() => null)) as {
        detail?: { message?: string };
      } | null;
      throw new Error(detail?.detail?.message ?? `audio control failed (${response.status})`);
    }
    return (await response.json()) as T;
  }

  /** The server's media clock now, in milliseconds. */
  private now(): number {
    return (this.serverClock + (performance.now() - this.clockOrigin)) >>> 0;
  }

  /** Attach, link main and playback, and start the speakers. */
  async start(): Promise<void> {
    if (this.main) return;
    this.stopped = false;
    this.update({ state: "connecting", error: null });
    try {
      const attach = await this.control<Attach>({ action: "attach" });
      this.token = attach.client_token;
      // The binding expires on the console's clock, so renew well inside it
      // rather than at the edge.
      this.renewTimer = window.setInterval(() => void this.renew(), (attach.expires_in * 1000) / 3);

      this.main = await SpiceChannel.link(this.url("main"), "main", 0);
      const connectionId = await new Promise<number>((resolve, reject) => {
        const timer = window.setTimeout(
          () => reject(new Error("the SPICE server sent no MSG_MAIN_INIT")),
          5000
        );
        this.main?.listen({
          onMessage: (message) => {
            this.mainMessage(message);
            if (message.kind === MSG_MAIN_INIT) {
              window.clearTimeout(timer);
              resolve(this.connectionId);
            }
          },
          onClose: (reason) => {
            window.clearTimeout(timer);
            reject(new Error(reason ?? "the main channel closed"));
            this.fail("The audio session ended; reconnect to restore it.");
          },
        });
      });
      this.main.send(MSGC_MAIN_ATTACH_CHANNELS, new Uint8Array(0));

      this.context = new AudioContext();
      if (this.context.state === "suspended") await this.context.resume();
      this.playback = new SpicePlayback(this.context, (stats) => this.update({ stats }));
      this.playbackChannel = await SpiceChannel.link(
        this.url("playback"),
        "playback",
        connectionId,
        {
          opus: true,
        }
      );
      this.playbackChannel.listen({
        onMessage: (message) => {
          void this.playback?.handle(message).catch((error: unknown) => {
            this.update({ error: error instanceof Error ? error.message : "playback failed" });
          });
        },
        onClose: () => this.update({ error: "The speaker channel closed." }),
      });
      this.update({ state: "connected", error: null });
    } catch (error) {
      this.fail(error instanceof Error ? error.message : "Guest audio could not be started");
    }
  }

  private url(channel: string): string {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    return `${scheme}://${location.host}/ws/v1/sessions/${encodeURIComponent(this.instanceId)}/${encodeURIComponent(this.sessionId)}/audio/${channel}?client_token=${encodeURIComponent(this.token ?? "")}`;
  }

  private mainMessage(message: SpiceMessage): void {
    if (message.kind === MSG_MAIN_INIT) {
      const init = parseMainInit(message.payload);
      this.connectionId = init.connectionId;
      this.serverClock = init.multiMediaTime;
      this.clockOrigin = performance.now();
      return;
    }
    if (message.kind === MSG_MAIN_MULTI_MEDIA_TIME && message.payload.length >= 4) {
      // Re-anchor the clock the record timestamps are expressed in, so a long
      // session does not drift away from the server's idea of the time.
      this.serverClock = new DataView(
        message.payload.buffer,
        message.payload.byteOffset,
        message.payload.byteLength
      ).getUint32(0, true);
      this.clockOrigin = performance.now();
    }
  }

  private async renew(): Promise<void> {
    if (!this.token || this.stopped) return;
    try {
      await this.control({ action: "renew", client_token: this.token });
    } catch {
      this.fail("The audio session expired; reconnect to restore it.");
    }
  }

  /** Take the session's capture lease and link the record channel. */
  async enableMicrophone(takeover = false): Promise<void> {
    if (!this.token || !this.main) throw new Error("guest audio is not connected");
    if (this.recordChannel) return;
    await this.control({
      action: "claim",
      client_token: this.token,
      ...(takeover ? { takeover: true } : {}),
    });
    this.update({ capture: true, error: null });
    // The lease is short on purpose; renew it for as long as the mic is on.
    this.captureTimer = window.setInterval(() => {
      void this.control({ action: "claim", client_token: this.token }).catch(() => {
        this.update({ error: "The microphone lease could not be renewed." });
        void this.disableMicrophone();
      });
    }, 20_000);
    try {
      this.recordChannel = await SpiceChannel.link(
        this.url("record"),
        "record",
        this.connectionId,
        {
          opus: true,
        }
      );
    } catch (error) {
      await this.disableMicrophone();
      throw error;
    }
    const channel = this.recordChannel;
    this.record = new SpiceRecord(
      (kind, payload) => channel.send(kind, payload),
      () => this.now(),
      channel.opus,
      (state: RecordState) => this.update({ guestListening: state.guestListening }),
      (error) => this.update({ error: error.message })
    );
    this.record.setMuted(this.status.microphoneMuted);
    channel.listen({
      onMessage: (message) => void this.record?.handle(message),
      onClose: () => {
        // The console closes this socket when the lease lapses or is taken,
        // so treat its closure as the lease being gone.
        this.record?.stop();
        this.record = null;
        this.recordChannel = null;
        this.clearCaptureTimer();
        this.update({ capture: false, guestListening: false });
      },
    });
  }

  /** Release the capture lease and close the record channel. */
  async disableMicrophone(): Promise<void> {
    this.clearCaptureTimer();
    this.record?.stop();
    this.record = null;
    this.recordChannel?.close();
    this.recordChannel = null;
    this.update({ capture: false, guestListening: false });
    if (this.token) {
      await this.control({ action: "release", client_token: this.token }).catch(() => {
        /* The lease may already have lapsed, which is the same outcome. */
      });
    }
  }

  setSpeakerMuted(muted: boolean): void {
    this.playback?.setMuted(muted);
    this.update({ speakerMuted: muted });
  }

  setMicrophoneMuted(muted: boolean): void {
    this.record?.setMuted(muted);
    this.update({ microphoneMuted: muted });
  }

  private clearCaptureTimer(): void {
    if (this.captureTimer !== null) window.clearInterval(this.captureTimer);
    this.captureTimer = null;
  }

  private fail(message: string): void {
    this.update({ state: "failed", error: message });
    void this.stop();
  }

  /** Close every channel and release the binding. */
  async stop(): Promise<void> {
    this.stopped = true;
    this.clearCaptureTimer();
    if (this.renewTimer !== null) window.clearInterval(this.renewTimer);
    this.renewTimer = null;
    this.record?.stop();
    this.record = null;
    this.playback?.teardown();
    this.playback = null;
    for (const channel of [this.recordChannel, this.playbackChannel, this.main]) channel?.close();
    this.recordChannel = null;
    this.playbackChannel = null;
    this.main = null;
    void this.context?.close();
    this.context = null;
    const token = this.token;
    this.token = null;
    if (token) {
      await fetch(`/api/v1/sessions/${encodeURIComponent(this.instanceId)}/${encodeURIComponent(this.sessionId)}/audio/control`, {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-MachineEmu-Token": document.querySelector<HTMLMetaElement>('meta[name="machineemu-token"]')?.content ?? "", "Idempotency-Key": crypto.randomUUID() },
        body: JSON.stringify({ action: "detach", client_token: token }),
      }).catch(() => {
        /* A closing tab cannot wait for this; the binding expires anyway. */
      });
    }
    if (this.status.state !== "failed") this.update({ ...IDLE });
  }
}
