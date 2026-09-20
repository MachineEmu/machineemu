"""Bounded RFB client-message filtering for the session VNC proxy."""

from __future__ import annotations


class RfbProtocolError(ValueError):
    """Raised when a browser sends an invalid or unsupported RFB message."""


class RfbInputGate:
    """Pass the RFB handshake and filter guest-directed input when view-only."""

    def __init__(self) -> None:
        self.buffer = bytearray()
        self.handshake_lengths = [12, 1, 1]

    def feed(self, data: bytes, *, allow_input: bool) -> bytes:
        if not isinstance(data, bytes):
            raise TypeError("RFB data must be bytes")
        output = bytearray()
        offset = 0
        while self.handshake_lengths and offset < len(data):
            needed = self.handshake_lengths[0]
            take = min(needed, len(data) - offset)
            output.extend(data[offset:offset + take])
            offset += take
            needed -= take
            if needed:
                self.handshake_lengths[0] = needed
            else:
                self.handshake_lengths.pop(0)
        self.buffer.extend(data[offset:])
        offset = 0
        if self.handshake_lengths:
            return bytes(output)
        while offset < len(self.buffer):
            message_type = self.buffer[offset]
            size = self._message_size(self.buffer, offset, message_type)
            if size is None or size > len(self.buffer) - offset:
                break
            frame = self.buffer[offset:offset + size]
            if allow_input or message_type not in {4, 5, 6, 251, 255}:
                output.extend(frame)
            offset += size
        if offset:
            del self.buffer[:offset]
        if len(self.buffer) > 1_048_576:
            raise RfbProtocolError("RFB client message exceeds the size limit")
        return bytes(output)

    @staticmethod
    def _message_size(data: bytearray, offset: int, message_type: int) -> int | None:
        available = len(data) - offset
        if message_type == 0:
            return 20
        if message_type == 2:
            if available < 4:
                return None
            return 4 + 4 * int.from_bytes(data[offset + 2:offset + 4], "big")
        if message_type == 3:
            return 10
        if message_type == 4:
            return 8
        if message_type == 5:
            if available < 2:
                return None
            return 7 if data[offset + 1] & 0x80 else 6
        if message_type == 6:
            if available < 8:
                return None
            length = abs(int.from_bytes(data[offset + 4:offset + 8], "big", signed=True))
            if length > 1_048_576 - 8:
                raise RfbProtocolError("RFB clipboard message exceeds the size limit")
            return 8 + length
        if message_type == 150:
            return 10
        if message_type == 248:
            if available < 9:
                return None
            return 9 + data[offset + 8]
        if message_type == 251:
            if available < 8:
                return None
            return 8 + 16 * data[offset + 6]
        if message_type == 255:
            return 12
        raise RfbProtocolError(f"unsupported RFB client message type {message_type}")
