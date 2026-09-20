#!/usr/bin/env python3
"""The UniFi mobile app's BLE setup tunnel, from the app side.

A factory-default console is provisioned over Bluetooth LE rather than over the
network: the app opens a GATT connection and tunnels HTTP-shaped requests
through a characteristic pair. This module speaks that tunnel, so the setup API
of an emulated console can be driven without the phone.

The layering is documented in `docs/ble-setup-protocol.md`, reconstructed from
the app; this is the encoder and decoder for it:

    GATT fragments
    └ uint16_be total_length (counts itself) | ciphertext
      └ secretbox: XSalsa20-Poly1305, 24-byte counter nonce
        └ int16_be sequence | uint8 protocol | payload
          ├ protocol 0 AUTHENTICATION -> MessagePack handshake arrays
          └ protocol 3 BINARY_MESSAGE -> Binme container -> UiComm v4 message
"""
from __future__ import annotations

import json
import struct
import time
import uuid
import zlib

import msgpack
from nacl.bindings import (crypto_generichash_blake2b_salt_personal,
                           crypto_scalarmult, crypto_scalarmult_base,
                           crypto_secretbox, crypto_secretbox_open)
from nacl.utils import random as random_bytes

# Hard-coded in the app (defpackage/js1.java:6); both directions of the
# handshake are secretboxed under it before a session key exists.
BOOTSTRAP_KEY = bytes.fromhex("a781f8a4a627373b70745738cdffdd1de9ae352517c374ca9afc215c39c62637")
AUTHENTICATION, MANAGEMENT, ALL_JOIN, BINARY_MESSAGE = 0, 1, 2, 3
HEADER_BLOCK, BODY_BLOCK = 1, 2
ENCODING_JSON, ENCODING_STRING, ENCODING_BINARY = 1, 2, 3
COMPRESSION_DISABLED, COMPRESSION_ENABLED = 0, 1
MAX_FRAME = 65535

# Service UUIDs the console advertises, and the characteristic pair the tunnel
# runs over (docs/ble-setup-protocol.md).
FACTORY_SERVICE = "afcad778-a44c-48d2-9b50-dbbaeff1e77a"
MANAGED_SERVICE = "26816cf6-334b-4580-bc3f-f1b72ef5d93e"
READER_CHARACTERISTIC = "d587c47f-ac6e-4388-a31c-e6cd380ba043"
WRITER_CHARACTERISTIC = "9280f26c-a56f-43ea-b769-d5d732e1ac67"

# The app's v4 codec names the setup calls `httpRequest`/`httpResponse`, but
# `ubnt-ble-http-transport` on UDM-Pro 5.1.19 accepts only the v2 spelling and
# says so: "Type other than 'request' is not supported". The field names are
# the same, so the pair is selectable per console.
REQUEST, RESPONSE = "request", "response"
HTTP_REQUEST, HTTP_RESPONSE = "httpRequest", "httpResponse"


class TunnelError(RuntimeError):
    """The peer spoke something this tunnel cannot decode."""


def blake2b_256(*parts: bytes) -> bytes:
    """Keyless BLAKE2b-256 over the concatenation, as the app derives it."""
    return crypto_generichash_blake2b_salt_personal(b"".join(parts), digest_size=32, key=b"")


def session_key(secret: bytes, peer_public: bytes,
                client_public: bytes, server_public: bytes) -> bytes:
    """X25519 then BLAKE2b-256(shared || client_public || server_public).

    Both ends derive the same key: each multiplies its own secret by the other's
    public key, and the two public keys are always concatenated app first.
    """
    return blake2b_256(crypto_scalarmult(secret, peer_public), client_public, server_public)


def frame(payload: bytes) -> bytes:
    """Length-delimited frame; the length counts its own two bytes."""
    total = len(payload) + 2
    if total > MAX_FRAME:
        raise TunnelError("frame exceeds the 16-bit length field")
    return struct.pack(">H", total) + payload


class FrameReader:
    """Reassemble length-delimited frames from a GATT byte stream."""

    def __init__(self) -> None:
        self.buffer = bytearray()

    def feed(self, chunk: bytes) -> list[bytes]:
        self.buffer.extend(chunk)
        frames = []
        while len(self.buffer) >= 2:
            total = struct.unpack_from(">H", self.buffer)[0]
            if total == 0:
                # A zero length is a no-op; the reader skips the two bytes.
                del self.buffer[:2]
                continue
            if total < 2:
                raise TunnelError(f"frame length {total} is shorter than its own field")
            if len(self.buffer) < total:
                break
            frames.append(bytes(self.buffer[2:total]))
            del self.buffer[:total]
        return frames


class Codec:
    """One key plus the two counter nonces that go with it.

    Transmit and receive keep separate counters, both seeded from the shared
    sequence counter when the codec is built, and each wraps at 16 bits.
    """

    def __init__(self, key: bytes, counter: int = 0) -> None:
        self.key = key
        self.transmit = counter & 0xFFFF
        self.receive = counter & 0xFFFF

    @staticmethod
    def nonce(counter: int) -> bytes:
        return struct.pack(">H", counter & 0xFFFF) + bytes(22)

    def seal(self, plaintext: bytes) -> bytes:
        sealed = crypto_secretbox(plaintext, self.nonce(self.transmit), self.key)
        self.transmit = (self.transmit + 1) & 0xFFFF
        return sealed

    def open(self, ciphertext: bytes) -> bytes:
        try:
            opened = crypto_secretbox_open(ciphertext, self.nonce(self.receive), self.key)
        except Exception as error:  # nacl raises CryptoError
            raise TunnelError(f"could not open a message at nonce {self.receive}: {error}") from error
        self.receive = (self.receive + 1) & 0xFFFF
        return opened


def envelope(sequence: int, protocol: int, payload: bytes) -> bytes:
    return struct.pack(">hB", sequence, protocol) + payload


def parse_envelope(packet: bytes) -> tuple[int, int, bytes]:
    if len(packet) < 2:
        raise TunnelError(f"Failed to parse Sequence Number from packet fragment of length {len(packet)}")
    if len(packet) < 3:
        raise TunnelError(f"Failed to parse Protocol from packet fragment of length {len(packet)}")
    sequence, protocol = struct.unpack_from(">hB", packet)
    if protocol not in (AUTHENTICATION, MANAGEMENT, ALL_JOIN, BINARY_MESSAGE):
        raise TunnelError(f"Unknown message protocol '{protocol}'")
    return sequence, protocol, packet[3:]


def binme(header: dict, body: bytes, *, encoding: int = ENCODING_JSON,
          compress: bool = False) -> bytes:
    """Serialize the two-block container the UiComm messages travel in."""
    def block(kind: int, data: bytes, block_encoding: int) -> bytes:
        payload = zlib.compress(data) if compress else data
        return (bytes([kind, block_encoding, COMPRESSION_ENABLED if compress else
                       COMPRESSION_DISABLED, 0]) + struct.pack(">I", len(payload)) + payload)

    return (block(HEADER_BLOCK, json.dumps(header, separators=(",", ":")).encode(), ENCODING_JSON)
            + block(BODY_BLOCK, body, encoding))


def parse_binme(message: bytes) -> tuple[dict, bytes]:
    """Return the JSON header and the body bytes, rejecting a bad container."""
    blocks, offset = [], 0
    while offset < len(message):
        if len(message) - offset < 8:
            raise TunnelError("truncated Binme block header")
        kind, encoding, compression, reserved = message[offset:offset + 4]
        length = struct.unpack_from(">I", message, offset + 4)[0]
        offset += 8
        if reserved != 0 or len(message) - offset < length:
            raise TunnelError("malformed Binme block")
        data = message[offset:offset + length]
        offset += length
        if compression == COMPRESSION_ENABLED:
            data = zlib.decompress(data)
        elif compression != COMPRESSION_DISABLED:
            raise TunnelError(f"unknown Binme compression {compression}")
        blocks.append((kind, encoding, data))
    if not blocks or blocks[0][0] != HEADER_BLOCK:
        raise TunnelError("a Binme message must start with a header block")
    if len(blocks) < 2 or blocks[1][0] != BODY_BLOCK:
        raise TunnelError("a Binme message must carry a body block")
    try:
        header = json.loads(blocks[0][2] or b"{}")
    except ValueError as error:
        raise TunnelError(f"invalid Binme header: {error}") from error
    return header, blocks[1][2]


class Tunnel:
    """The app side of the setup tunnel over any byte-stream transport.

    `transport` only has to `send(bytes)` the fragments and hand whatever
    arrives to `feed(bytes)`; `att_central.py` wires that to GATT.
    """

    def __init__(self, transport, *, client_secret: bytes | None = None) -> None:
        self.transport = transport
        self.reader = FrameReader()
        self.sequence = 0
        self.client_secret = client_secret or random_bytes(32)
        self.client_public = crypto_scalarmult_base(self.client_secret)
        self.server_public: bytes | None = None
        self.codec = Codec(BOOTSTRAP_KEY, self.sequence)
        self.session: Codec | None = None
        self.pending: list[tuple[int, bytes]] = []
        self.identifier = 0

    # Wire ----------------------------------------------------------------

    def _next_sequence(self) -> int:
        value = self.sequence
        self.sequence = (self.sequence + 1) & 0xFFFF
        return struct.unpack(">h", struct.pack(">H", value))[0]

    def send(self, protocol: int, payload: bytes) -> None:
        codec = self.session or self.codec
        self.transport.send(frame(codec.seal(envelope(self._next_sequence(), protocol, payload))))

    def feed(self, chunk: bytes) -> None:
        """Hand incoming fragments in; decoded packets queue up for receive()."""
        codec = self.session or self.codec
        for raw in self.reader.feed(chunk):
            _, protocol, payload = parse_envelope(codec.open(raw))
            self.pending.append((protocol, payload))

    def receive(self, protocol: int | None = None) -> bytes:
        """Take the next decoded packet, optionally requiring its protocol."""
        if not self.pending:
            raise TunnelError("no message is pending; feed the transport first")
        kind, payload = self.pending.pop(0)
        if protocol is not None and kind != protocol:
            raise TunnelError(f"expected protocol {protocol}, received {kind}")
        return payload

    # Handshake -----------------------------------------------------------

    def start_handshake(self) -> None:
        """Step 1: offer the ephemeral public key under the bootstrap key."""
        self.send(AUTHENTICATION, msgpack.packb(["DHPK", False, self.client_public],
                                                use_bin_type=True))

    def accept_server_key(self) -> bytes:
        """Step 2: take the device's public key, positionally validated."""
        message = msgpack.unpackb(self.receive(AUTHENTICATION), raw=False)
        if (not isinstance(message, list) or len(message) != 3 or message[0] != "DHPK"
                or message[1] is not True or not isinstance(message[2], bytes)):
            raise TunnelError("Failed to parse DH server public key")
        self.server_public = message[2]
        return self.server_public

    def authenticate(self) -> None:
        """Step 3: name the key that keys this session."""
        self.send(AUTHENTICATION, msgpack.packb(["AUTH", "DH", self.client_public],
                                                use_bin_type=True))

    def accept_authentication(self) -> None:
        """Step 4: the device confirms, and both sides move to the session key."""
        message = msgpack.unpackb(self.receive(AUTHENTICATION), raw=False)
        if not isinstance(message, list) or len(message) != 2 or message[:2] != ["AUTH", "DH"]:
            raise TunnelError("Failed to parse DH authentication response")
        if self.server_public is None:
            raise TunnelError("the device has not sent its public key")
        # The session codec is built with the sequence counter as it stands, so
        # its nonces do not start at zero.
        self.session = Codec(session_key(self.client_secret, self.server_public,
                                         self.client_public, self.server_public), self.sequence)

    # HTTP over the tunnel -------------------------------------------------

    def next_identifier(self) -> str:
        """`new UUID(0L, counter++)`, as the app mints request identifiers."""
        value = self.identifier
        self.identifier += 1
        return str(uuid.UUID(int=value))

    def http_request(self, method: str, path: str, body: object = None,
                     headers: dict | None = None, kind: str = REQUEST) -> str | int:
        """Send one request; returns the identifier its response carries.

        The two spellings correlate differently: the app's v4 `httpRequest`
        carries a UUID `id`, while the console's v2 `request` wants a long
        integer under `requestId` ("A long integer of 'requestId' key was not
        found" is what it says otherwise).
        """
        if self.session is None:
            raise TunnelError("the session key is not established")
        common = {"method": method.upper(), "path": path, "headers": headers or {}}
        if kind == HTTP_REQUEST:
            identifier: str | int = self.next_identifier()
            header = {"type": kind, "id": identifier,
                      "timestamp": int(time.time() * 1000), **common}
        else:
            identifier = self.identifier
            self.identifier += 1
            header = {"type": kind, "requestId": identifier, **common}
        payload = b"" if body is None else json.dumps(body, separators=(",", ":")).encode()
        self.send(BINARY_MESSAGE, binme(header, payload))
        return identifier

    def http_response(self) -> tuple[dict, object]:
        """Decode the next response, returning its header and parsed body."""
        header, body = parse_binme(self.receive(BINARY_MESSAGE))
        if header.get("type") not in (RESPONSE, HTTP_RESPONSE):
            raise TunnelError(f"expected a response, received {header.get('type')!r}")
        if not body:
            return header, None
        try:
            return header, json.loads(body)
        except ValueError:
            return header, body
