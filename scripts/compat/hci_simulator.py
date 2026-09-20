#!/usr/bin/env python3
"""A simulated HCI controller for the emulated UDM Pro's second UART.

The board exports ttyS1 as a QEMU chardev, and the guest attaches BlueZ to it
with `btattach -P h4`, so whatever speaks H4 on the host end of that socket is
the guest's `hci0`. `bt-uart-bridge.py` puts a real controller there; this
daemon puts a modelled one there instead, so a session gets Bluetooth without
a physical adapter, without root and without touching the host's own stack.

Like the hwsim daemon, one process serves several sessions: each instance has
its own socket, its own controller state and its own simulated peers, and they
share one datagram control socket that reports and tunes them by name.

It models the subset BlueZ uses to bring a controller up and advertise: the
initialization reads, the event masks, the BR/EDR knobs bluetoothd writes, and
legacy LE advertising and scanning. Unknown commands are answered with the
"unknown HCI command" status rather than silence, which is what a real
controller does for anything it does not implement.
"""
from __future__ import annotations

import argparse
from collections import deque
import errno
import json
import os
from pathlib import Path
import selectors
import socket
import struct
import sys
import time

H4_COMMAND, H4_ACL, H4_SCO, H4_EVENT = 1, 2, 3, 4
EVT_DISCONNECT_COMPLETE = 0x05
EVT_COMMAND_COMPLETE = 0x0E
EVT_COMMAND_STATUS = 0x0F
EVT_LE_META = 0x3E
LE_ADVERTISING_REPORT = 0x02
STATUS_OK = 0x00
STATUS_UNKNOWN_COMMAND = 0x01
STATUS_INVALID_PARAMETERS = 0x12
MAX_LINE = 65536
MAX_PACKET = 4096
RECENT_COMMANDS = 32

# Identity of the CSR 8811-class part the vendor firmware expects to find on
# this port: HCI 4.0, Cambridge Silicon Radio, legacy advertising only.
HCI_VERSION, HCI_REVISION = 0x06, 0x22BB
LMP_VERSION, LMP_SUBVERSION, MANUFACTURER = 0x06, 0x22BB, 10
LOCAL_FEATURES = bytes.fromhex("ffff8ffedbff5b87")
LE_FEATURES = struct.pack("<Q", 0x0000000000000001)
LE_STATES = struct.pack("<Q", 0x000003FFFFFFFFFF)
ACL_MTU, ACL_PACKETS, SCO_MTU, SCO_PACKETS = 310, 10, 64, 8
LE_MTU, LE_PACKETS = 27, 4
WHITE_LIST_SIZE = 25
GIAC = bytes.fromhex("338b9e")  # General Inquiry Access Code, little-endian


def opcode(group: int, command: int) -> int:
    return (group << 10) | command


RESET = opcode(0x03, 0x0003)
SET_EVENT_MASK = opcode(0x03, 0x0001)
SET_EVENT_FILTER = opcode(0x03, 0x0005)
FLUSH = opcode(0x03, 0x0008)
READ_LOCAL_NAME = opcode(0x03, 0x0014)
WRITE_LOCAL_NAME = opcode(0x03, 0x0013)
READ_SCAN_ENABLE = opcode(0x03, 0x0019)
WRITE_SCAN_ENABLE = opcode(0x03, 0x001A)
READ_CLASS_OF_DEVICE = opcode(0x03, 0x0023)
WRITE_CLASS_OF_DEVICE = opcode(0x03, 0x0024)
READ_VOICE_SETTING = opcode(0x03, 0x0025)
WRITE_VOICE_SETTING = opcode(0x03, 0x0026)
HOST_BUFFER_SIZE = opcode(0x03, 0x0033)
SET_FLOW_CONTROL = opcode(0x03, 0x0031)
WRITE_INQUIRY_MODE = opcode(0x03, 0x0045)
WRITE_PAGE_SCAN_TYPE = opcode(0x03, 0x0047)
WRITE_INQUIRY_SCAN_TYPE = opcode(0x03, 0x0043)
WRITE_EXTENDED_INQUIRY_RESPONSE = opcode(0x03, 0x0052)
READ_SIMPLE_PAIRING_MODE = opcode(0x03, 0x0055)
WRITE_SIMPLE_PAIRING_MODE = opcode(0x03, 0x0056)
READ_INQUIRY_RESPONSE_TX_POWER = opcode(0x03, 0x0058)
READ_LE_HOST_SUPPORTED = opcode(0x03, 0x006C)
WRITE_LE_HOST_SUPPORTED = opcode(0x03, 0x006D)
SET_EVENT_MASK_PAGE_2 = opcode(0x03, 0x0063)
WRITE_SECURE_CONNECTIONS = opcode(0x03, 0x007A)
READ_CONNECTION_ACCEPT_TIMEOUT = opcode(0x03, 0x0015)
WRITE_CONNECTION_ACCEPT_TIMEOUT = opcode(0x03, 0x0016)
WRITE_AUTHENTICATION_ENABLE = opcode(0x03, 0x0020)
READ_NUMBER_OF_SUPPORTED_IAC = opcode(0x03, 0x0038)
READ_CURRENT_IAC_LAP = opcode(0x03, 0x0039)
READ_LOCAL_VERSION = opcode(0x04, 0x0001)
READ_LOCAL_COMMANDS = opcode(0x04, 0x0002)
READ_LOCAL_FEATURES = opcode(0x04, 0x0003)
READ_LOCAL_EXTENDED_FEATURES = opcode(0x04, 0x0004)
READ_BUFFER_SIZE = opcode(0x04, 0x0005)
READ_BD_ADDR = opcode(0x04, 0x0009)
LE_SET_EVENT_MASK = opcode(0x08, 0x0001)
LE_READ_BUFFER_SIZE = opcode(0x08, 0x0002)
LE_READ_LOCAL_FEATURES = opcode(0x08, 0x0003)
LE_SET_RANDOM_ADDRESS = opcode(0x08, 0x0005)
LE_SET_ADVERTISING_PARAMETERS = opcode(0x08, 0x0006)
LE_READ_ADVERTISING_TX_POWER = opcode(0x08, 0x0007)
LE_SET_ADVERTISING_DATA = opcode(0x08, 0x0008)
LE_SET_SCAN_RESPONSE_DATA = opcode(0x08, 0x0009)
LE_SET_ADVERTISE_ENABLE = opcode(0x08, 0x000A)
LE_SET_SCAN_PARAMETERS = opcode(0x08, 0x000B)
LE_SET_SCAN_ENABLE = opcode(0x08, 0x000C)
LE_READ_WHITE_LIST_SIZE = opcode(0x08, 0x000F)
LE_CLEAR_WHITE_LIST = opcode(0x08, 0x0010)
LE_RAND = opcode(0x08, 0x0018)
LE_READ_SUPPORTED_STATES = opcode(0x08, 0x001C)
DISCONNECT = opcode(0x01, 0x0006)
READ_REMOTE_VERSION = opcode(0x01, 0x001D)
LE_CONNECTION_UPDATE = opcode(0x08, 0x0013)
LE_READ_REMOTE_FEATURES = opcode(0x08, 0x0016)

# Bit position in Read Local Supported Commands of everything this controller
# implements, so the host only asks for what is modelled here. The positions
# are BlueZ's own (monitor/packet.c opcode table), not derived by hand: a wrong
# bit silently advertises a neighbouring command and the host then sends one
# this model would refuse.
SUPPORTED_COMMANDS = {
    DISCONNECT: 5,
    READ_REMOTE_VERSION: 23,
    SET_EVENT_MASK: 46,
    RESET: 47,
    SET_EVENT_FILTER: 48,
    FLUSH: 49,
    WRITE_LOCAL_NAME: 56,
    READ_CONNECTION_ACCEPT_TIMEOUT: 58,
    WRITE_CONNECTION_ACCEPT_TIMEOUT: 59,
    READ_LOCAL_NAME: 57,
    READ_SCAN_ENABLE: 62,
    WRITE_SCAN_ENABLE: 63,
    WRITE_AUTHENTICATION_ENABLE: 69,
    READ_CLASS_OF_DEVICE: 72,
    WRITE_CLASS_OF_DEVICE: 73,
    READ_VOICE_SETTING: 74,
    WRITE_VOICE_SETTING: 75,
    SET_FLOW_CONTROL: 85,
    READ_NUMBER_OF_SUPPORTED_IAC: 90,
    READ_CURRENT_IAC_LAP: 91,
    HOST_BUFFER_SIZE: 86,
    WRITE_INQUIRY_SCAN_TYPE: 101,
    WRITE_INQUIRY_MODE: 103,
    WRITE_PAGE_SCAN_TYPE: 105,
    READ_LOCAL_VERSION: 115,
    READ_LOCAL_COMMANDS: 116,
    READ_LOCAL_FEATURES: 117,
    READ_LOCAL_EXTENDED_FEATURES: 118,
    READ_BUFFER_SIZE: 119,
    READ_BD_ADDR: 121,
    WRITE_EXTENDED_INQUIRY_RESPONSE: 137,
    READ_SIMPLE_PAIRING_MODE: 141,
    WRITE_SIMPLE_PAIRING_MODE: 142,
    READ_INQUIRY_RESPONSE_TX_POWER: 144,
    SET_EVENT_MASK_PAGE_2: 178,
    READ_LE_HOST_SUPPORTED: 197,
    WRITE_LE_HOST_SUPPORTED: 198,
    LE_SET_EVENT_MASK: 200,
    LE_READ_BUFFER_SIZE: 201,
    LE_READ_LOCAL_FEATURES: 202,
    LE_SET_RANDOM_ADDRESS: 204,
    LE_SET_ADVERTISING_PARAMETERS: 205,
    LE_READ_ADVERTISING_TX_POWER: 206,
    LE_SET_ADVERTISING_DATA: 207,
    LE_SET_SCAN_RESPONSE_DATA: 208,
    LE_SET_ADVERTISE_ENABLE: 209,
    LE_SET_SCAN_PARAMETERS: 210,
    LE_SET_SCAN_ENABLE: 211,
    LE_READ_WHITE_LIST_SIZE: 214,
    LE_CLEAR_WHITE_LIST: 215,
    LE_CONNECTION_UPDATE: 218,
    LE_READ_REMOTE_FEATURES: 221,
    LE_RAND: 223,
    LE_READ_SUPPORTED_STATES: 227,
    WRITE_SECURE_CONNECTIONS: 259,
}


def address_bytes(text: str) -> bytes:
    """Little-endian BD_ADDR as HCI carries it, from the printed form."""
    parts = text.split(":")
    if len(parts) != 6 or any(len(part) != 2 for part in parts):
        raise ValueError(f"invalid Bluetooth address: {text}")
    return bytes(int(part, 16) for part in reversed(parts))


def address_text(raw: bytes) -> str:
    if len(raw) != 6:
        raise ValueError("a Bluetooth address is six bytes")
    return ":".join(f"{byte:02x}" for byte in reversed(raw))


class Controller:
    """One modelled controller: the state BlueZ reads back and writes."""

    def __init__(self, address="00:1a:7d:00:00:01", name="UniFi lab controller"):
        self.address = address
        self.default_name = name
        self.commands = 0
        self.events = 0
        self.acl_from_host = 0
        self.unknown = 0
        self.recent = deque(maxlen=RECENT_COMMANDS)
        self.pending_disconnect: tuple[int, int] | None = None
        self.reset()

    def reset(self):
        self.event_mask = b"\x00" * 8
        self.event_mask_page2 = b"\x00" * 8
        self.le_event_mask = b"\x00" * 8
        self.name = self.default_name
        self.class_of_device = b"\x00\x00\x00"
        self.voice_setting = 0x0060
        self.connection_accept_timeout = 0x7D00
        self.authentication = 0
        self.scan_enable = 0
        self.inquiry_mode = 0
        self.simple_pairing = 0
        self.secure_connections = 0
        self.le_host_supported = 0
        self.random_address = "00:00:00:00:00:00"
        self.advertising = False
        self.advertising_interval = (0x0800, 0x0800)
        self.advertising_type = 0
        self.advertising_data = b""
        self.scan_response_data = b""
        self.scanning = False
        self.scan_type = 0
        self.white_list: list[str] = []

    @property
    def supported_commands(self) -> bytes:
        table = bytearray(64)
        for bit in SUPPORTED_COMMANDS.values():
            table[bit // 8] |= 1 << (bit % 8)
        return bytes(table)

    def status(self) -> dict:
        return {"address": self.address, "name": self.name,
                "class_of_device": self.class_of_device[::-1].hex(),
                "scan_enable": self.scan_enable,
                "simple_pairing": bool(self.simple_pairing),
                "le_host_supported": bool(self.le_host_supported),
                "advertising": self.advertising,
                "advertising_type": self.advertising_type,
                "advertising_data": self.advertising_data.hex(),
                "scan_response_data": self.scan_response_data.hex(),
                "scanning": self.scanning,
                "random_address": self.random_address,
                "white_list": list(self.white_list),
                "counters": {"commands": self.commands, "events": self.events,
                             "acl_from_host": self.acl_from_host, "unknown": self.unknown},
                "recent_commands": [f"0x{value:04x}" for value in self.recent]}

    # Command handling -----------------------------------------------------

    def complete(self, code: int, payload: bytes = b"") -> bytes:
        self.events += 1
        body = struct.pack("<BH", 1, code) + payload
        return bytes([H4_EVENT, EVT_COMMAND_COMPLETE, len(body)]) + body

    def event(self, code: int, payload: bytes) -> bytes:
        self.events += 1
        return bytes([H4_EVENT, code, len(payload)]) + payload

    def le_event(self, subevent: int, payload: bytes) -> bytes:
        return self.event(EVT_LE_META, bytes([subevent]) + payload)

    def status_event(self, code: int, status: int = STATUS_OK) -> bytes:
        self.events += 1
        return bytes([H4_EVENT, EVT_COMMAND_STATUS, 4, status, 1]) + struct.pack("<H", code)

    def command(self, code: int, params: bytes) -> list[bytes]:
        """Answer one HCI command packet, as a list of event packets."""
        self.commands += 1
        self.recent.append(code)
        handler = HANDLERS.get(code)
        if handler is None:
            self.unknown += 1
            return [self.complete(code, bytes([STATUS_UNKNOWN_COMMAND]))]
        return handler(self, code, params)

    def completed_packets(self, handle: int, count: int = 1) -> bytes:
        """Return the host's ACL credit, or it stops sending after a few frames."""
        return self.event(0x13, struct.pack("<BHH", 1, handle, count))

    def advertising_report(self, peer: dict) -> bytes:
        """One LE advertising report, as a scanning host would receive it."""
        data = bytes.fromhex(peer.get("data", ""))
        payload = (bytes([1, peer.get("event_type", 0), peer.get("address_type", 0)])
                   + address_bytes(peer["address"]) + bytes([len(data)]) + data
                   + struct.pack("b", peer.get("rssi", -60)))
        return self.le_event(LE_ADVERTISING_REPORT, payload)


def _ok(controller: Controller, code: int, payload: bytes = b"") -> list[bytes]:
    return [controller.complete(code, bytes([STATUS_OK]) + payload)]


def _store(field: str, length: int | None = None, status_only: bool = True):
    """Handler for a write command whose parameters are kept verbatim."""
    def handler(controller: Controller, code: int, params: bytes) -> list[bytes]:
        if length is not None and len(params) != length:
            return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
        setattr(controller, field, params)
        return _ok(controller, code)
    return handler


def _reset(controller: Controller, code: int, params: bytes) -> list[bytes]:
    controller.reset()
    return _ok(controller, code)


def _read_local_version(controller: Controller, code: int, params: bytes) -> list[bytes]:
    return _ok(controller, code, struct.pack("<BHBHH", HCI_VERSION, HCI_REVISION,
                                             LMP_VERSION, MANUFACTURER, LMP_SUBVERSION))


def _read_local_extended_features(controller: Controller, code: int, params: bytes) -> list[bytes]:
    page = params[0] if params else 0
    features = LOCAL_FEATURES if page == 0 else b"\x00" * 8
    return _ok(controller, code, bytes([page, 2 if page == 0 else page]) + features)


def _read_buffer_size(controller: Controller, code: int, params: bytes) -> list[bytes]:
    return _ok(controller, code, struct.pack("<HBHH", ACL_MTU, SCO_MTU, ACL_PACKETS, SCO_PACKETS))


def _read_bd_addr(controller: Controller, code: int, params: bytes) -> list[bytes]:
    return _ok(controller, code, address_bytes(controller.address))


def _read_local_name(controller: Controller, code: int, params: bytes) -> list[bytes]:
    encoded = controller.name.encode("utf-8")[:247]
    return _ok(controller, code, encoded + bytes(248 - len(encoded)))


def _write_local_name(controller: Controller, code: int, params: bytes) -> list[bytes]:
    controller.name = params.split(b"\x00", 1)[0].decode("utf-8", errors="replace")
    return _ok(controller, code)


def _read_class_of_device(controller: Controller, code: int, params: bytes) -> list[bytes]:
    return _ok(controller, code, controller.class_of_device)


def _write_class_of_device(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 3:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.class_of_device = params
    return _ok(controller, code)


def _byte_field(field: str, readable: bool):
    def handler(controller: Controller, code: int, params: bytes) -> list[bytes]:
        if readable:
            return _ok(controller, code, bytes([getattr(controller, field)]))
        if len(params) != 1:
            return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
        setattr(controller, field, params[0])
        return _ok(controller, code)
    return handler


def _read_voice_setting(controller: Controller, code: int, params: bytes) -> list[bytes]:
    return _ok(controller, code, struct.pack("<H", controller.voice_setting))


def _write_voice_setting(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 2:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.voice_setting = struct.unpack("<H", params)[0]
    return _ok(controller, code)


def _read_le_host_supported(controller: Controller, code: int, params: bytes) -> list[bytes]:
    return _ok(controller, code, bytes([controller.le_host_supported, 0]))


def _write_le_host_supported(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 2:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.le_host_supported = params[0]
    return _ok(controller, code)


def _le_read_buffer_size(controller: Controller, code: int, params: bytes) -> list[bytes]:
    return _ok(controller, code, struct.pack("<HB", LE_MTU, LE_PACKETS))


def _le_set_advertising_parameters(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 15:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    minimum, maximum, kind = struct.unpack_from("<HHB", params)
    controller.advertising_interval = (minimum, maximum)
    controller.advertising_type = kind
    return _ok(controller, code)


def _le_set_advertising_data(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 32:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.advertising_data = params[1:1 + params[0]]
    return _ok(controller, code)


def _le_set_scan_response_data(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 32:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.scan_response_data = params[1:1 + params[0]]
    return _ok(controller, code)


def _le_set_advertise_enable(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 1 or params[0] > 1:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.advertising = params[0] == 1
    return _ok(controller, code)


def _le_set_scan_parameters(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 7:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.scan_type = params[0]
    return _ok(controller, code)


def _le_set_scan_enable(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 2 or params[0] > 1:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.scanning = params[0] == 1
    return _ok(controller, code)


def _le_set_random_address(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 6:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.random_address = address_text(params)
    return _ok(controller, code)


def _le_rand(controller: Controller, code: int, params: bytes) -> list[bytes]:
    return _ok(controller, code, os.urandom(8))


def _write_connection_accept_timeout(controller: Controller, code: int, params: bytes) -> list[bytes]:
    if len(params) != 2:
        return [controller.complete(code, bytes([STATUS_INVALID_PARAMETERS]))]
    controller.connection_accept_timeout = struct.unpack("<H", params)[0]
    return _ok(controller, code)


def _disconnect(controller: Controller, code: int, params: bytes) -> list[bytes]:
    """The host tears the link down; the instance finishes the exchange."""
    if len(params) != 3:
        return [controller.status_event(code, STATUS_INVALID_PARAMETERS)]
    handle, reason = struct.unpack("<HB", params)
    controller.pending_disconnect = (handle, reason)
    return [controller.status_event(code)]


HANDLERS = {
    RESET: _reset,
    # The kernel sends these unconditionally while bringing a BR/EDR
    # controller up, so refusing them fails the whole initialization.
    READ_CONNECTION_ACCEPT_TIMEOUT: lambda controller, code, params: _ok(
        controller, code, struct.pack("<H", controller.connection_accept_timeout)),
    WRITE_CONNECTION_ACCEPT_TIMEOUT: _write_connection_accept_timeout,
    WRITE_AUTHENTICATION_ENABLE: _byte_field("authentication", False),
    READ_NUMBER_OF_SUPPORTED_IAC: lambda controller, code, params: _ok(
        controller, code, bytes([1])),
    READ_CURRENT_IAC_LAP: lambda controller, code, params: _ok(
        controller, code, bytes([1]) + GIAC),
    SET_EVENT_MASK: _store("event_mask", 8),
    SET_EVENT_MASK_PAGE_2: _store("event_mask_page2", 8),
    SET_EVENT_FILTER: lambda controller, code, params: _ok(controller, code),
    FLUSH: lambda controller, code, params: _ok(controller, code, params[:2]),
    READ_LOCAL_VERSION: _read_local_version,
    READ_LOCAL_COMMANDS: lambda controller, code, params: _ok(
        controller, code, controller.supported_commands),
    READ_LOCAL_FEATURES: lambda controller, code, params: _ok(controller, code, LOCAL_FEATURES),
    READ_LOCAL_EXTENDED_FEATURES: _read_local_extended_features,
    READ_BUFFER_SIZE: _read_buffer_size,
    READ_BD_ADDR: _read_bd_addr,
    READ_LOCAL_NAME: _read_local_name,
    WRITE_LOCAL_NAME: _write_local_name,
    READ_CLASS_OF_DEVICE: _read_class_of_device,
    WRITE_CLASS_OF_DEVICE: _write_class_of_device,
    READ_SCAN_ENABLE: _byte_field("scan_enable", True),
    WRITE_SCAN_ENABLE: _byte_field("scan_enable", False),
    READ_VOICE_SETTING: _read_voice_setting,
    WRITE_VOICE_SETTING: _write_voice_setting,
    HOST_BUFFER_SIZE: lambda controller, code, params: _ok(controller, code),
    SET_FLOW_CONTROL: lambda controller, code, params: _ok(controller, code),
    WRITE_INQUIRY_MODE: _byte_field("inquiry_mode", False),
    WRITE_INQUIRY_SCAN_TYPE: lambda controller, code, params: _ok(controller, code),
    WRITE_PAGE_SCAN_TYPE: lambda controller, code, params: _ok(controller, code),
    WRITE_EXTENDED_INQUIRY_RESPONSE: lambda controller, code, params: _ok(controller, code),
    READ_SIMPLE_PAIRING_MODE: _byte_field("simple_pairing", True),
    WRITE_SIMPLE_PAIRING_MODE: _byte_field("simple_pairing", False),
    WRITE_SECURE_CONNECTIONS: _byte_field("secure_connections", False),
    READ_INQUIRY_RESPONSE_TX_POWER: lambda controller, code, params: _ok(
        controller, code, struct.pack("b", 0)),
    READ_LE_HOST_SUPPORTED: _read_le_host_supported,
    WRITE_LE_HOST_SUPPORTED: _write_le_host_supported,
    LE_SET_EVENT_MASK: _store("le_event_mask", 8),
    LE_READ_BUFFER_SIZE: _le_read_buffer_size,
    LE_READ_LOCAL_FEATURES: lambda controller, code, params: _ok(controller, code, LE_FEATURES),
    LE_READ_SUPPORTED_STATES: lambda controller, code, params: _ok(controller, code, LE_STATES),
    LE_READ_ADVERTISING_TX_POWER: lambda controller, code, params: _ok(
        controller, code, struct.pack("b", 7)),
    LE_READ_WHITE_LIST_SIZE: lambda controller, code, params: _ok(
        controller, code, bytes([WHITE_LIST_SIZE])),
    LE_CLEAR_WHITE_LIST: lambda controller, code, params: _ok(controller, code),
    LE_SET_RANDOM_ADDRESS: _le_set_random_address,
    LE_SET_ADVERTISING_PARAMETERS: _le_set_advertising_parameters,
    LE_SET_ADVERTISING_DATA: _le_set_advertising_data,
    LE_SET_SCAN_RESPONSE_DATA: _le_set_scan_response_data,
    LE_SET_ADVERTISE_ENABLE: _le_set_advertise_enable,
    LE_SET_SCAN_PARAMETERS: _le_set_scan_parameters,
    LE_SET_SCAN_ENABLE: _le_set_scan_enable,
    LE_RAND: _le_rand,
    # Link commands are answered with a status here; the Instance owns the
    # connection and emits the completion event that follows.
    DISCONNECT: _disconnect,
    READ_REMOTE_VERSION: lambda controller, code, params: [
        controller.status_event(code),
        controller.event(0x0C, struct.pack("<BHBHH", STATUS_OK,
                                           struct.unpack_from("<H", params)[0] if len(params) >= 2 else 0,
                                           LMP_VERSION, MANUFACTURER, LMP_SUBVERSION))],
    LE_CONNECTION_UPDATE: lambda controller, code, params: [
        controller.status_event(code),
        controller.le_event(0x03, struct.pack("<BHHHH", STATUS_OK,
                                              struct.unpack_from("<H", params)[0] if len(params) >= 2 else 0,
                                              0x0028, 0x0000, 0x002A))],
    LE_READ_REMOTE_FEATURES: lambda controller, code, params: [
        controller.status_event(code),
        controller.le_event(0x04, bytes([STATUS_OK])
                            + (params[:2] if len(params) >= 2 else b"\x00\x00") + LE_FEATURES)],
}


class Transport:
    """H4 framing over one stream: whole packets in, event packets out.

    Guest ACL fragments are reassembled into complete L2CAP frames and left in
    `acl_frames` for the caller to route to whoever is on the other end of the
    link; HCI commands are answered here.
    """

    def __init__(self, controller: Controller):
        self.controller = controller
        self.incoming = bytearray()
        self.acl_frames: list[tuple[int, bytes]] = []
        self.assembling: dict[int, bytearray] = {}

    def acl(self, handle_flags: int, payload: bytes) -> list[bytes]:
        """Reassemble one ACL fragment; complete frames land in `acl_frames`."""
        handle = handle_flags & 0x0FFF
        continuation = (handle_flags >> 12) & 0x03 == 0x01
        self.controller.acl_from_host += 1
        buffer = self.assembling.get(handle) if continuation else None
        if buffer is None:
            if continuation:
                # A continuation without a first fragment is not recoverable.
                return [self.controller.completed_packets(handle)]
            buffer = bytearray()
            self.assembling[handle] = buffer
        buffer.extend(payload)
        if len(buffer) >= 4:
            total = struct.unpack_from("<H", buffer)[0] + 4
            if len(buffer) >= total:
                self.acl_frames.append((handle, bytes(buffer[:total])))
                del self.assembling[handle]
        return [self.controller.completed_packets(handle)]

    def feed(self, chunk: bytes) -> list[bytes]:
        """Consume host bytes, returning the controller's replies."""
        self.incoming.extend(chunk)
        if len(self.incoming) > MAX_PACKET * 4:
            raise ValueError("host sent more than a packet backlog of data")
        replies: list[bytes] = []
        while self.incoming:
            kind = self.incoming[0]
            if kind == H4_COMMAND:
                if len(self.incoming) < 4:
                    break
                length = self.incoming[3]
                if len(self.incoming) < 4 + length:
                    break
                code = struct.unpack_from("<H", self.incoming, 1)[0]
                params = bytes(self.incoming[4:4 + length])
                del self.incoming[:4 + length]
                replies.extend(self.controller.command(code, params))
            elif kind in (H4_ACL, H4_SCO):
                header = 5 if kind == H4_ACL else 4
                if len(self.incoming) < header:
                    break
                length = (struct.unpack_from("<H", self.incoming, 3)[0] if kind == H4_ACL
                          else self.incoming[3])
                if len(self.incoming) < header + length:
                    break
                handle_flags = struct.unpack_from("<H", self.incoming, 1)[0]
                payload = bytes(self.incoming[header:header + length])
                del self.incoming[:header + length]
                if kind == H4_ACL:
                    replies.extend(self.acl(handle_flags, payload))
            else:
                # A real controller resynchronizes rather than stalling; the
                # vendor bring-up path can leave BCSP bytes on this port.
                del self.incoming[:1]
        return replies


class Peer:
    """A simulated central: L2CAP frames as JSON lines, one connection at a time."""

    def __init__(self, path: Path):
        self.path = Path(path)
        self.server: socket.socket | None = None
        self.client: socket.socket | None = None
        self.incoming = bytearray()
        self.output: deque[bytes] = deque()
        self.address = "00:aa:bb:cc:dd:ee"
        self.address_type = 1  # Random, which is what phones advertise with.

    def listen(self) -> None:
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        old_umask = os.umask(0o177)
        try:
            self.server.bind(str(self.path))
        finally:
            os.umask(old_umask)
        self.server.listen(1)
        self.server.setblocking(False)

    def attach(self, client: socket.socket) -> None:
        self.client = client
        self.client.setblocking(False)
        self.incoming = bytearray()
        self.output = deque()

    def detach(self) -> None:
        if self.client is not None:
            self.client.close()
        self.client = None
        self.incoming = bytearray()
        self.output = deque()

    def send(self, message: dict) -> None:
        if self.client is None:
            return
        if len(self.output) >= 256:
            raise ValueError("central output queue full")
        self.output.append((json.dumps({"version": 1, **message}) + "\n").encode())

    def close(self) -> None:
        self.detach()
        if self.server is not None:
            self.server.close()
            self.server = None
        self.path.unlink(missing_ok=True)


class Instance:
    """One session: its chardev connection, controller and simulated central."""

    def __init__(self, name, path, address=None, peer_path=None, controller_name=None):
        self.name = name
        self.path = Path(path)
        self.controller = Controller(address or "00:1a:7d:00:00:01",
                                     controller_name or f"UniFi lab {name}")
        self.transport = Transport(self.controller)
        self.peer = Peer(peer_path) if peer_path else None
        self.link: socket.socket | None = None
        self.output: deque[bytes] = deque()
        self.attachments = 0
        self.handle: int | None = None
        self.assembling: dict[int, bytearray] = {}
        self.expected = 0
        self.last_error: str | None = None

    @property
    def attached(self) -> bool:
        return self.link is not None

    # Guest link ----------------------------------------------------------

    def connect(self) -> bool:
        """Try once to reach the QEMU chardev; it is a server, we are a client."""
        if self.link is not None:
            return True
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            client.connect(str(self.path))
        except OSError as error:
            client.close()
            if error.errno not in (errno.ENOENT, errno.ECONNREFUSED, errno.EAGAIN):
                self.last_error = str(error)
            return False
        client.setblocking(False)
        self.link = client
        self.attachments += 1
        self.last_error = None
        self.transport = Transport(self.controller)
        return True

    def disconnect(self) -> None:
        if self.link is not None:
            self.link.close()
        self.link = None
        self.output = deque()
        self.transport = Transport(self.controller)
        self.drop_connection()

    def write(self, packets) -> None:
        for packet in packets:
            if len(self.output) >= 512:
                raise ValueError("controller output queue full")
            self.output.append(packet)

    def feed(self, chunk: bytes) -> None:
        """Host bytes: HCI commands answer here, ACL goes to the central."""
        self.write(self.transport.feed(chunk))
        frames, self.transport.acl_frames = self.transport.acl_frames, []
        for handle, payload in frames:
            self.forward(handle, payload)
        requested = self.controller.pending_disconnect
        if requested is not None:
            self.controller.pending_disconnect = None
            self.drop_connection(requested[1] if requested[0] == self.handle else 0x02)

    # BLE connection ------------------------------------------------------

    def open_connection(self) -> bool:
        """A simulated central connects to the advertising guest."""
        if self.handle is not None or not self.controller.advertising or self.link is None:
            return False
        self.handle = 0x0040
        self.controller.advertising = False
        peer = self.peer
        self.write([self.controller.le_event(0x01, struct.pack(
            "<BHBB6sHHHB", STATUS_OK, self.handle, 0x01,
            peer.address_type if peer else 1,
            address_bytes(peer.address if peer else "00:aa:bb:cc:dd:ee"),
            0x0028, 0x0000, 0x002A, 0x00))])
        if peer:
            peer.send({"type": "connected", "handle": self.handle,
                       "address": self.controller.address, "instance": self.name})
        return True

    def drop_connection(self, reason: int = 0x13) -> None:
        if self.handle is None:
            return
        handle, self.handle = self.handle, None
        self.assembling.clear()
        if self.link is not None:
            self.write([self.controller.event(EVT_DISCONNECT_COMPLETE,
                                              struct.pack("<BHB", STATUS_OK, handle, reason))])
        if self.peer:
            self.peer.send({"type": "disconnected", "handle": handle, "reason": reason})

    def forward(self, handle: int, payload: bytes) -> None:
        if self.peer is None or handle != self.handle:
            return
        cid = struct.unpack_from("<H", payload, 2)[0] if len(payload) >= 4 else 0
        self.peer.send({"type": "l2cap", "handle": handle, "cid": cid,
                        "data": payload[4:].hex()})

    def from_central(self, message: dict) -> None:
        """One JSON line from the simulated central."""
        if not isinstance(message, dict) or message.get("version") != 1:
            raise ValueError("expected central protocol version 1")
        kind = message.get("type")
        if kind == "connect":
            if not self.open_connection():
                raise ValueError("the guest is not advertising")
            return
        if kind == "disconnect":
            self.drop_connection(int(message.get("reason", 0x13)))
            return
        if kind != "l2cap":
            raise ValueError("unsupported central operation")
        if self.handle is None:
            raise ValueError("no LE connection is open")
        data = bytes.fromhex(message.get("data", ""))
        cid = message.get("cid", 4)
        if type(cid) is not int or not 0 < cid < 0x10000 or len(data) > 512:
            raise ValueError("invalid L2CAP frame")
        self.write(list(self.acl_packets(struct.pack("<HH", len(data), cid) + data)))

    def acl_packets(self, frame: bytes):
        """Fragment one L2CAP frame into HCI ACL packets the host can take."""
        first = True
        while frame:
            chunk, frame = frame[:LE_MTU], frame[LE_MTU:]
            flags = 0x00 if first else 0x01
            first = False
            yield (bytes([H4_ACL]) + struct.pack("<HH", (self.handle & 0x0FFF) | (flags << 12),
                                                 len(chunk)) + chunk)

    def status(self) -> dict:
        return {"instance": self.name, "socket": str(self.path),
                "peer_socket": str(self.peer.path) if self.peer else None,
                "attached": self.attached, "attachments": self.attachments,
                "central_connected": bool(self.peer and self.peer.client is not None),
                "link_handle": self.handle, "error": self.last_error,
                "controller": self.controller.status()}

    def control(self, request: dict) -> dict:
        operation = request.get("type")
        if operation == "stats":
            return {"version": 1, "type": "stats", **self.status()}
        if operation == "advertise":
            # Inject a nearby advertiser, which a scanning guest reports to BlueZ.
            if not self.controller.scanning:
                raise ValueError("the guest is not scanning")
            peer = request.get("peer")
            if not isinstance(peer, dict) or not isinstance(peer.get("address"), str):
                raise ValueError("an advertising peer needs an address")
            self.write([self.controller.advertising_report(peer)])
            return {"version": 1, "type": "advertised", **self.status()}
        if operation == "configure":
            settings = request.get("settings")
            if not isinstance(settings, dict) or set(settings) - {"address", "name"}:
                raise ValueError("only address and name can be configured")
            if "address" in settings:
                address_bytes(settings["address"])
                self.controller.address = settings["address"]
            if "name" in settings:
                if not isinstance(settings["name"], str):
                    raise ValueError("name must be a string")
                self.controller.default_name = settings["name"]
            return {"version": 1, "type": "configured", **self.status()}
        raise ValueError("unsupported control operation")

    def close(self) -> None:
        self.disconnect()
        if self.peer:
            self.peer.close()


def control_request(instances, request):
    """Route one control datagram to the addressed instance."""
    if not isinstance(request, dict) or request.get("version") != 1:
        raise ValueError("expected control protocol version 1")
    if request.get("type") == "list":
        return {"version": 1, "type": "instances",
                "instances": [instance.status() for instance in instances]}
    name = request.get("instance")
    if name is None:
        if len(instances) != 1:
            raise ValueError("this daemon serves several instances; name one")
        return instances[0].control(request)
    if not isinstance(name, str):
        raise ValueError("instance must be a name")
    for instance in instances:
        if instance.name == name:
            return instance.control(request)
    raise ValueError(f"unknown instance: {name}")


def load_instances(path):
    """Read the daemon's instance table: name, chardev socket and identity."""
    document = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(document, list) or not document:
        raise ValueError("instance table must be a nonempty list")
    instances, paths, addresses = [], set(), set()
    allowed = {"name", "socket", "address", "peer_socket", "controller_name"}
    for entry in document:
        if not isinstance(entry, dict) or set(entry) - allowed:
            raise ValueError("each instance needs name and socket, and may set "
                             "address, peer_socket and controller_name")
        name, socket_path = entry.get("name"), entry.get("socket")
        if not isinstance(name, str) or not name or any(item.name == name for item in instances):
            raise ValueError("instance names must be unique and nonempty")
        if not isinstance(socket_path, str) or not socket_path or socket_path in paths:
            raise ValueError("instance socket paths must be unique and nonempty")
        address = entry.get("address")
        if address is not None:
            address_bytes(address)
            if address in addresses:
                raise ValueError("controller addresses must be unique")
            addresses.add(address)
        peer_socket = entry.get("peer_socket")
        if peer_socket is not None and (not isinstance(peer_socket, str) or peer_socket in paths):
            raise ValueError("peer socket paths must be unique and nonempty")
        paths.add(socket_path)
        if peer_socket:
            paths.add(peer_socket)
        instances.append(Instance(name, socket_path, address, peer_socket,
                                  entry.get("controller_name")))
    return instances


def serve(instances, control=None, stop=None, poll=0.2):
    """Run every instance: guest links, simulated centrals and control."""
    for instance in instances:
        if instance.peer:
            instance.peer.listen()
    registered = {}
    with selectors.DefaultSelector() as selector:
        if control is not None:
            control.setblocking(False)
            selector.register(control, selectors.EVENT_READ, ("control", None))
        for instance in instances:
            if instance.peer:
                selector.register(instance.peer.server, selectors.EVENT_READ,
                                  ("listen", instance))
        try:
            print(f"serving {len(instances)} controller(s): "
                  + ", ".join(f"{instance.name}={instance.path}" for instance in instances),
                  file=sys.stderr, flush=True)
            next_attempt = 0.0
            while stop is None or not stop():
                now = time.monotonic()
                if now >= next_attempt:
                    next_attempt = now + 1
                    for instance in instances:
                        # QEMU owns the chardev socket, so the daemon keeps
                        # trying until a session exists and after it restarts.
                        if not instance.attached and instance.connect():
                            selector.register(instance.link, selectors.EVENT_READ,
                                              ("link", instance))
                            registered[instance.name] = instance.link
                for instance in instances:
                    link = registered.get(instance.name)
                    if link is not None and instance.link is link:
                        selector.modify(link, selectors.EVENT_READ |
                                        (selectors.EVENT_WRITE if instance.output else 0),
                                        ("link", instance))
                    if instance.peer and instance.peer.client is not None:
                        selector.modify(instance.peer.client, selectors.EVENT_READ |
                                        (selectors.EVENT_WRITE if instance.peer.output else 0),
                                        ("central", instance))
                for key, mask in selector.select(poll):
                    role, instance = key.data
                    if role == "control":
                        raw, address = control.recvfrom(MAX_LINE)
                        try:
                            response = control_request(instances, json.loads(raw))
                        except (ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
                            response = {"version": 1, "type": "error", "error": str(error)}
                        if address:
                            control.sendto(json.dumps(response).encode(), address)
                    elif role == "listen":
                        client, _ = instance.peer.server.accept()
                        if instance.peer.client is not None:
                            client.close()
                            continue
                        instance.peer.attach(client)
                        selector.register(client, selectors.EVENT_READ, ("central", instance))
                    elif role == "link":
                        # A guest that vanishes resets the socket rather than
                        # closing it cleanly; that is a disconnect, not a fault.
                        try:
                            alive = _pump_link(instance, mask)
                        except OSError:
                            alive = False
                        if not alive:
                            selector.unregister(instance.link)
                            registered.pop(instance.name, None)
                            instance.disconnect()
                    else:
                        try:
                            alive = _pump_central(instance, mask)
                        except OSError:
                            alive = False
                        if not alive:
                            selector.unregister(instance.peer.client)
                            instance.drop_connection(0x16)
                            instance.peer.detach()
        finally:
            for instance in instances:
                instance.close()


def _pump_link(instance, mask):
    """Move one guest link's bytes; False once QEMU has closed the chardev."""
    if mask & selectors.EVENT_READ:
        chunk = instance.link.recv(4096)
        if not chunk:
            return False
        instance.feed(chunk)
    if mask & selectors.EVENT_WRITE and instance.output:
        sent = instance.link.send(instance.output[0])
        instance.output[0] = instance.output[0][sent:]
        if not instance.output[0]:
            instance.output.popleft()
    return True


def _pump_central(instance, mask):
    """Move one simulated central's JSON lines; False once it disconnects."""
    peer = instance.peer
    if mask & selectors.EVENT_READ:
        chunk = peer.client.recv(4096)
        if not chunk:
            return False
        peer.incoming.extend(chunk)
        if len(peer.incoming) > MAX_LINE:
            raise ValueError("central message too large")
        while b"\n" in peer.incoming:
            line, _, remainder = peer.incoming.partition(b"\n")
            peer.incoming = bytearray(remainder)
            try:
                instance.from_central(json.loads(line))
            except (ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
                peer.send({"type": "error", "error": str(error)})
    if mask & selectors.EVENT_WRITE and peer.output:
        sent = peer.client.send(peer.output[0])
        peer.output[0] = peer.output[0][sent:]
        if not peer.output[0]:
            peer.output.popleft()
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--socket", type=Path,
                        help="QEMU chardev socket backing the guest's ttyS1")
    parser.add_argument("--peer-socket", type=Path,
                        help="where a simulated central attaches for this controller")
    parser.add_argument("--address", default="00:1a:7d:00:00:01", help="controller BD_ADDR")
    parser.add_argument("--name", help="controller local name")
    parser.add_argument("--instances", type=Path,
                        help="JSON instance table: serve several sessions from this daemon")
    parser.add_argument("--control", type=Path, help="private Unix datagram control socket")
    args = parser.parse_args()
    if bool(args.instances) == bool(args.socket):
        parser.error("serving requires either --socket or --instances")
    if args.instances:
        instances = load_instances(args.instances)
    else:
        instances = [Instance("default", args.socket, args.address, args.peer_socket, args.name)]
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control:
        if args.control:
            old_umask = os.umask(0o177)
            try:
                control.bind(str(args.control))
            finally:
                os.umask(old_umask)
        try:
            serve(instances, control if args.control else None)
        finally:
            if args.control:
                args.control.unlink(missing_ok=True)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
    except (OSError, ValueError, KeyError, struct.error) as error:
        sys.exit(f"hci simulator: {error}")
