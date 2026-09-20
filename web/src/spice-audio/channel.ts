/**
 * One SPICE channel over one WebSocket.
 *
 * SPICE does not multiplex channels onto a byte stream, so neither does this:
 * main, playback, and record each get their own socket to the console, which
 * gives each its own upstream connection to QEMU.
 */
import {
  AUTH_SELECTION_SPICE,
  COMMON_CAP_MINI_HEADER,
  LINK_ERR_OK,
  LINK_HEADER_BYTES,
  MSGC_ACK_SYNC,
  MSGC_PONG,
  MSG_PING,
  MSG_SET_ACK,
  PLAYBACK_CAP_OPUS,
  RECORD_CAP_OPUS,
  TICKET_BYTES,
  type ChannelName,
} from "./protocol";
import {
  Framer,
  SpiceError,
  encodeLinkHeader,
  encodeLinkMess,
  encodeMessage,
  hasCap,
  parseLinkHeader,
  parseLinkReply,
  type SpiceMessage,
} from "./codec";

/**
 * Encrypt the SPICE ticket with the server's RSA key.
 *
 * The server asks for this 128-byte RSA-OAEP-SHA1 blob even when it was
 * started with `disable-ticketing=on`, in which case the password is not
 * checked and the empty one is conventional. WebCrypto will import the
 * 162-byte DER `SubjectPublicKeyInfo` the server sends directly.
 */
async function encryptTicket(publicKey: Uint8Array, password: string): Promise<Uint8Array> {
  const key = await crypto.subtle.importKey(
    "spki",
    publicKey.slice().buffer as ArrayBuffer,
    { name: "RSA-OAEP", hash: "SHA-1" },
    false,
    ["encrypt"]
  );
  const plain = new TextEncoder().encode(password);
  // SPICE tickets are NUL-terminated C strings.
  const terminated = new Uint8Array(plain.length + 1);
  terminated.set(plain);
  const cipher = new Uint8Array(
    await crypto.subtle.encrypt({ name: "RSA-OAEP" }, key, terminated.buffer as ArrayBuffer)
  );
  if (cipher.length !== TICKET_BYTES) throw new SpiceError("SPICE ticket is not 128 bytes");
  return cipher;
}

type Handlers = {
  onMessage: (message: SpiceMessage) => void;
  onClose: (reason: string | null) => void;
};

/** A linked SPICE channel; `link` resolves once the handshake has completed. */
export class SpiceChannel {
  private socket: WebSocket;
  private framer = new Framer();
  private handlers: Handlers | null = null;
  private phase: "reply" | "result" | "messages" = "reply";
  private buffer = new Uint8Array(0);
  private replySize = 0;
  private settle: ((error: Error | null) => void) | null = null;
  /** Whether Opus was agreed for this channel. */
  opus = false;

  private constructor(
    socket: WebSocket,
    readonly channel: ChannelName
  ) {
    this.socket = socket;
  }

  /**
   * Open a channel and complete its link handshake.
   *
   * `connectionId` is zero for the first main link and the value from
   * `MSG_MAIN_INIT` afterwards. The console proxy checks it against the id it
   * observed on this audio client's main channel, so a mismatch is refused by
   * the server side as well as here.
   */
  static async link(
    url: string,
    channel: ChannelName,
    connectionId: number,
    options: { password?: string; opus?: boolean } = {}
  ): Promise<SpiceChannel> {
    const socket = new WebSocket(url);
    socket.binaryType = "arraybuffer";
    const spice = new SpiceChannel(socket, channel);
    const wantOpus = options.opus ?? false;
    await new Promise<void>((resolve, reject) => {
      spice.settle = (error) => {
        spice.settle = null;
        if (error) reject(error);
        else resolve();
      };
      socket.onerror = () =>
        spice.settle?.(new SpiceError(`the ${channel} channel could not connect`));
      socket.onclose = (event) => {
        const reason = event.reason || `code ${event.code}`;
        spice.settle?.(new SpiceError(`the ${channel} channel closed during link: ${reason}`));
        spice.handlers?.onClose(reason);
      };
      socket.onmessage = (event) => {
        void spice.receive(
          new Uint8Array(event.data as ArrayBuffer),
          options.password ?? "",
          wantOpus
        );
      };
      socket.onopen = () => {
        const mess = encodeLinkMess(channel, connectionId, wantOpus);
        socket.send(encodeLinkHeader(mess.length));
        socket.send(mess);
      };
    });
    return spice;
  }

  /** Deliver messages and closures to the caller once the link has completed. */
  listen(handlers: Handlers): void {
    this.handlers = handlers;
  }

  send(kind: number, payload: Uint8Array): void {
    if (this.socket.readyState === WebSocket.OPEN) this.socket.send(encodeMessage(kind, payload));
  }

  close(): void {
    this.handlers = null;
    this.framer.clear();
    if (this.socket.readyState <= WebSocket.OPEN) this.socket.close();
  }

  get open(): boolean {
    return this.socket.readyState === WebSocket.OPEN;
  }

  private append(chunk: Uint8Array): void {
    const grown = new Uint8Array(this.buffer.length + chunk.length);
    grown.set(this.buffer);
    grown.set(chunk, this.buffer.length);
    this.buffer = grown;
  }

  private async receive(chunk: Uint8Array, password: string, wantOpus: boolean): Promise<void> {
    try {
      this.append(chunk);
      // The handshake is length-driven: reply header, reply body, ticket
      // result, then messages. Nothing is guessed from message boundaries.
      if (this.phase === "reply") {
        if (this.buffer.length < LINK_HEADER_BYTES) return;
        if (this.replySize === 0) this.replySize = parseLinkHeader(this.buffer).size;
        if (this.buffer.length < LINK_HEADER_BYTES + this.replySize) return;
        const reply = parseLinkReply(
          this.buffer.slice(LINK_HEADER_BYTES, LINK_HEADER_BYTES + this.replySize)
        );
        this.buffer = this.buffer.slice(LINK_HEADER_BYTES + this.replySize);
        if (reply.error !== LINK_ERR_OK) {
          throw new SpiceError(
            `the SPICE server refused the ${this.channel} link (${reply.error})`
          );
        }
        if (!hasCap(reply.commonCaps, COMMON_CAP_MINI_HEADER)) {
          throw new SpiceError("the SPICE server declined mini headers");
        }
        const opusCap = this.channel === "record" ? RECORD_CAP_OPUS : PLAYBACK_CAP_OPUS;
        this.opus = wantOpus && this.channel !== "main" && hasCap(reply.channelCaps, opusCap);
        const selection = new Uint8Array(4);
        new DataView(selection.buffer).setUint32(0, AUTH_SELECTION_SPICE, true);
        this.socket.send(selection);
        this.socket.send(await encryptTicket(reply.publicKey, password));
        this.phase = "result";
      }
      if (this.phase === "result") {
        if (this.buffer.length < 4) return;
        const code = new DataView(
          this.buffer.buffer,
          this.buffer.byteOffset,
          this.buffer.byteLength
        ).getUint32(0, true);
        this.buffer = this.buffer.slice(4);
        if (code !== LINK_ERR_OK) {
          throw new SpiceError(`the SPICE ticket was refused on ${this.channel} (${code})`);
        }
        this.phase = "messages";
        this.settle?.(null);
      }
      if (this.buffer.length > 0) {
        const rest = this.buffer;
        this.buffer = new Uint8Array(0);
        for (const message of this.framer.feed(rest)) {
          if (this.answerFlowControl(message)) continue;
          this.handlers?.onMessage(message);
        }
      }
    } catch (error) {
      const failure = error instanceof Error ? error : new SpiceError("SPICE channel failed");
      if (this.settle) this.settle(failure);
      else this.handlers?.onClose(failure.message);
      this.close();
    }
  }

  /**
   * Answer the server's acknowledgement windows and latency probes.
   *
   * A channel that never replies to `MSG_SET_ACK` is throttled by the server
   * once its window fills, which shows up as audio that starts and then
   * stops. Returns true when the message needs no further handling.
   */
  private answerFlowControl(message: SpiceMessage): boolean {
    if (message.kind === MSG_SET_ACK) {
      this.send(MSGC_ACK_SYNC, message.payload.slice(0, 4));
      return true;
    }
    if (message.kind === MSG_PING) {
      this.send(MSGC_PONG, message.payload);
      return true;
    }
    return false;
  }
}
