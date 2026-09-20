"""Fail-closed SPICE audio framing gates for the browser proxy."""

from __future__ import annotations

MAGIC = b"REDQ"
LINK_HEADER_BYTES = 16
LINK_MESS_BYTES = 18
LINK_REPLY_BYTES = 178
TICKET_BYTES = 128
MAX_LINK_BYTES = 4096
MAX_MESSAGE_BYTES = 512 * 1024
MAX_CLIENT_MESSAGE_BYTES = 64 * 1024
MINI_HEADER_BYTES = 6
CHANNEL_TYPES = {"main": 1, "playback": 5, "record": 6}
COMMON_CAP_MINI_HEADER = 3
COMMON_CAP_AUTH_SPICE = 1
COMMON_CAP_AUTH_SASL = 2
AUTH_SELECTION_SPICE = 1
MSG_MAIN_INIT = 103
COMMON_CLIENT_MESSAGES = {1, 2, 3, 6}
CLIENT_MESSAGES = {
    "main": COMMON_CLIENT_MESSAGES | {101, 104},
    "playback": COMMON_CLIENT_MESSAGES,
    "record": COMMON_CLIENT_MESSAGES | {101, 102, 103},
}


class SpiceProtocolError(ValueError):
    pass


def _u16(data: bytes, offset: int) -> int:
    return int.from_bytes(data[offset:offset + 2], "little")


def _u32(data: bytes, offset: int) -> int:
    return int.from_bytes(data[offset:offset + 4], "little")


def _caps(body: bytes, offset: int, count: int) -> list[int]:
    if count > MAX_LINK_BYTES // 4 or offset > MAX_LINK_BYTES or offset + count * 4 > len(body):
        raise SpiceProtocolError("SPICE capabilities exceed the link frame")
    return [_u32(body, offset + index * 4) for index in range(count)]


def _has(words: list[int], bit: int) -> bool:
    return bit // 32 < len(words) and bool(words[bit // 32] & (1 << (bit % 32)))


class SpiceClientGate:
    def __init__(self, channel: str, connection_id: int):
        if channel not in CHANNEL_TYPES:
            raise SpiceProtocolError("unsupported SPICE audio channel")
        self.channel = channel
        self.connection_id = connection_id
        self.buffer = bytearray()
        self.phase = "header"
        self.link_size = 0

    def feed(self, data: bytes) -> bytes:
        self.buffer.extend(data)
        if len(self.buffer) > MAX_LINK_BYTES + MAX_CLIENT_MESSAGE_BYTES + MINI_HEADER_BYTES:
            raise SpiceProtocolError("SPICE client buffer exceeds the limit")
        output = bytearray()
        while self._step(output):
            pass
        return bytes(output)

    def _step(self, output: bytearray) -> bool:
        if self.phase == "header":
            if len(self.buffer) < LINK_HEADER_BYTES:
                return False
            header = bytes(self.buffer[:LINK_HEADER_BYTES])
            if header[:4] != MAGIC or _u32(header, 4) != 2:
                raise SpiceProtocolError("unsupported SPICE link header")
            self.link_size = _u32(header, 12)
            if not LINK_MESS_BYTES <= self.link_size <= MAX_LINK_BYTES:
                raise SpiceProtocolError("SPICE link message length is out of range")
            output.extend(header)
            del self.buffer[:LINK_HEADER_BYTES]
            self.phase = "message"
            return True
        if self.phase == "message":
            if len(self.buffer) < self.link_size:
                return False
            body = bytes(self.buffer[:self.link_size])
            self._check_link(body)
            output.extend(body)
            del self.buffer[:self.link_size]
            self.phase = "auth"
            return True
        if self.phase == "auth":
            if len(self.buffer) < 4:
                return False
            if _u32(bytes(self.buffer), 0) != AUTH_SELECTION_SPICE:
                raise SpiceProtocolError("only SPICE ticket authentication is carried")
            output.extend(self.buffer[:4])
            del self.buffer[:4]
            self.phase = "ticket"
            return True
        if self.phase == "ticket":
            if len(self.buffer) < TICKET_BYTES:
                return False
            output.extend(self.buffer[:TICKET_BYTES])
            del self.buffer[:TICKET_BYTES]
            self.phase = "messages"
            return True
        if len(self.buffer) < MINI_HEADER_BYTES:
            return False
        kind, size = _u16(self.buffer, 0), _u32(self.buffer, 2)
        if size > MAX_CLIENT_MESSAGE_BYTES or kind not in CLIENT_MESSAGES[self.channel]:
            raise SpiceProtocolError("SPICE client message is not permitted")
        if len(self.buffer) < MINI_HEADER_BYTES + size:
            return False
        output.extend(self.buffer[:MINI_HEADER_BYTES + size])
        del self.buffer[:MINI_HEADER_BYTES + size]
        return True

    def _check_link(self, body: bytes) -> None:
        if _u32(body, 0) != self.connection_id or body[4] != CHANNEL_TYPES[self.channel] or body[5] != 0:
            raise SpiceProtocolError("SPICE channel identity does not match the authorized route")
        common_count, channel_count, offset = _u32(body, 6), _u32(body, 10), _u32(body, 14)
        common = _caps(body, offset, common_count)
        _caps(body, offset + common_count * 4, channel_count)
        if not _has(common, COMMON_CAP_MINI_HEADER) or not _has(common, COMMON_CAP_AUTH_SPICE) or _has(common, COMMON_CAP_AUTH_SASL):
            raise SpiceProtocolError("SPICE audio link capabilities are not permitted")


class SpiceServerGate:
    def __init__(self, channel: str):
        self.channel = channel
        self.buffer = bytearray()
        self.phase = "header"
        self.reply_size = 0
        self.connection_id: int | None = None

    def feed(self, data: bytes) -> None:
        self.buffer.extend(data)
        if len(self.buffer) > MAX_LINK_BYTES + MAX_MESSAGE_BYTES + MINI_HEADER_BYTES:
            raise SpiceProtocolError("SPICE server buffer exceeds the limit")
        while self._step():
            pass

    def _step(self) -> bool:
        if self.phase == "header":
            if len(self.buffer) < LINK_HEADER_BYTES:
                return False
            header = bytes(self.buffer[:LINK_HEADER_BYTES])
            if header[:4] != MAGIC:
                raise SpiceProtocolError("SPICE server link magic is invalid")
            self.reply_size = _u32(header, 12)
            if not LINK_REPLY_BYTES <= self.reply_size <= MAX_LINK_BYTES:
                raise SpiceProtocolError("SPICE link reply length is out of range")
            del self.buffer[:LINK_HEADER_BYTES]
            self.phase = "reply"
            return True
        if self.phase == "reply":
            if len(self.buffer) < self.reply_size:
                return False
            body = bytes(self.buffer[:self.reply_size])
            if _u32(body, 0) != 0:
                raise SpiceProtocolError("SPICE server refused the link")
            common_count, channel_count, offset = _u32(body, 166), _u32(body, 170), _u32(body, 174)
            if not _has(_caps(body, offset, common_count), COMMON_CAP_MINI_HEADER):
                raise SpiceProtocolError("SPICE server declined mini headers")
            _caps(body, offset + common_count * 4, channel_count)
            del self.buffer[:self.reply_size]
            self.phase = "result"
            return True
        if self.phase == "result":
            if len(self.buffer) < 4:
                return False
            if _u32(bytes(self.buffer), 0) != 0:
                raise SpiceProtocolError("SPICE server refused the channel")
            del self.buffer[:4]
            self.phase = "messages"
            return True
        if len(self.buffer) < MINI_HEADER_BYTES:
            return False
        kind, size = _u16(self.buffer, 0), _u32(self.buffer, 2)
        if size > MAX_MESSAGE_BYTES or len(self.buffer) < MINI_HEADER_BYTES + size:
            if size > MAX_MESSAGE_BYTES:
                raise SpiceProtocolError("SPICE server message exceeds the limit")
            return False
        if self.channel == "main" and kind == MSG_MAIN_INIT:
            if size < 32:
                raise SpiceProtocolError("SPICE main init is too short")
            self.connection_id = _u32(self.buffer, MINI_HEADER_BYTES)
        del self.buffer[:MINI_HEADER_BYTES + size]
        return True
