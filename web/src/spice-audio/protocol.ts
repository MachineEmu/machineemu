/**
 * SPICE wire constants for the browser audio client.
 *
 * Written from the permissively licensed `spice-protocol` definitions, not
 * from spice-html5, which is LGPL and would pull copyleft into this bundle.
 * These values mirror `crates/spice-audio/src/protocol.rs` and
 * `console/spice.py`; all three are checked against the golden vectors in
 * `tests/fixtures/spice-audio.json`.
 */

/** Link header magic, `REDQ` read as a little-endian word. */
export const MAGIC = 0x51444552;
export const VERSION_MAJOR = 2;
export const VERSION_MINOR = 2;

export const LINK_HEADER_BYTES = 16;
export const LINK_MESS_BYTES = 18;
export const LINK_REPLY_BYTES = 178;
export const TICKET_PUBKEY_BYTES = 162;
export const TICKET_BYTES = 128;
export const MINI_HEADER_BYTES = 6;

export const MAX_LINK_BYTES = 4096;
export const MAX_MESSAGE_BYTES = 512 * 1024;

/** The only three channels this client speaks. */
export const CHANNEL = { main: 1, playback: 5, record: 6 } as const;
export type ChannelName = keyof typeof CHANNEL;

export const COMMON_CAP_PROTOCOL_AUTH_SELECTION = 0;
export const COMMON_CAP_AUTH_SPICE = 1;
export const COMMON_CAP_AUTH_SASL = 2;
export const COMMON_CAP_MINI_HEADER = 3;
export const AUTH_SELECTION_SPICE = 1;

export const PLAYBACK_CAP_OPUS = 3;
export const RECORD_CAP_OPUS = 2;

export const LINK_ERR_OK = 0;

/** Flow-control messages every channel shares. */
export const MSG_SET_ACK = 3;
export const MSG_PING = 4;
export const MSG_DISCONNECTING = 6;
export const MSG_NOTIFY = 7;
export const MSGC_ACK_SYNC = 1;
export const MSGC_ACK = 2;
export const MSGC_PONG = 3;

export const MSG_MAIN_INIT = 103;
export const MSG_MAIN_CHANNELS_LIST = 104;
export const MSG_MAIN_MULTI_MEDIA_TIME = 106;
export const MSGC_MAIN_ATTACH_CHANNELS = 104;

export const MSG_PLAYBACK_DATA = 101;
export const MSG_PLAYBACK_MODE = 102;
export const MSG_PLAYBACK_START = 103;
export const MSG_PLAYBACK_STOP = 104;
export const MSG_PLAYBACK_VOLUME = 105;
export const MSG_PLAYBACK_MUTE = 106;
export const MSG_PLAYBACK_LATENCY = 107;

export const MSG_RECORD_START = 101;
export const MSG_RECORD_STOP = 102;
export const MSG_RECORD_VOLUME = 103;
export const MSG_RECORD_MUTE = 104;
export const MSGC_RECORD_DATA = 101;
export const MSGC_RECORD_MODE = 102;
export const MSGC_RECORD_START_MARK = 103;

/** Audio coding modes. CELT is recognized but never negotiated. */
export const AUDIO_MODE = { raw: 1, celt: 2, opus: 3 } as const;
export type AudioMode = (typeof AUDIO_MODE)[keyof typeof AUDIO_MODE];

/** Signed 16-bit little-endian samples, the only format SPICE defines. */
export const AUDIO_FMT_S16 = 1;
