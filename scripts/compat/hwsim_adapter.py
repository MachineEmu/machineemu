#!/usr/bin/env python3
"""Experimental raw-802.11 backend for the UniFi vendor compatibility frontend.

This is a medium bridge, NOT an MT7981 firmware/descriptor implementation.
Run only inside a dedicated hwsim network namespace. No modules, radios,
interfaces, firmware images, or network configuration are changed by this tool.
"""

import argparse
from collections import deque
import heapq
import json
import os
from pathlib import Path
import selectors
import signal
import socket
import struct
import sys
import time
import random


REGISTER, FRAME, TX_INFO = 1, 2, 3
RECEIVER, TRANSMITTER, DATA, FLAGS = 1, 2, 3, 4
RX_RATE, SIGNAL, RATES, COOKIE, FREQ = 5, 6, 7, 8, 19
NO_ACK, ACK = 2, 4
MAX_LINE, MAX_PENDING = 16384, 256


class Medium:
    """Deterministic RF settings shared by both directions of the bridge."""

    def __init__(self, signal=-40, jitter=0, loss=0, latency_ms=0,
                 rate_index=None, aggregate=False, seed=1):
        if not -110 <= signal <= 0 or not 0 <= jitter <= 60:
            raise ValueError("invalid signal profile")
        if not 0 <= loss <= 1 or not 0 <= latency_ms <= 60000:
            raise ValueError("invalid loss or latency profile")
        if rate_index is not None and not 0 <= rate_index < 32:
            raise ValueError("invalid hwsim rate index")
        self.signal_dbm = signal
        self.jitter_db = jitter
        self.loss = loss
        self.latency = latency_ms / 1000
        self.rate_index = rate_index
        self.aggregate = aggregate
        self.random = random.Random(seed)

    def settings(self):
        return {"signal": self.signal_dbm, "jitter": self.jitter_db,
                "loss": self.loss, "latency_ms": round(self.latency * 1000),
                "rate_index": self.rate_index, "aggregate": self.aggregate}

    def reconfigured(self, changes):
        if not isinstance(changes, dict):
            raise ValueError("settings must be an object")
        allowed = {"signal", "jitter", "loss", "latency_ms", "rate_index",
                   "aggregate", "seed"}
        if set(changes) - allowed:
            raise ValueError("unknown medium setting")
        current = self.settings()
        current.update(changes)
        if type(current["signal"]) is not int or type(current["jitter"]) is not int:
            raise ValueError("signal and jitter must be integers")
        if type(current["loss"]) not in (int, float):
            raise ValueError("loss must be numeric")
        if type(current["latency_ms"]) is not int:
            raise ValueError("latency_ms must be an integer")
        if current["rate_index"] is not None and type(current["rate_index"]) is not int:
            raise ValueError("rate_index must be an integer or null")
        if type(current["aggregate"]) is not bool:
            raise ValueError("aggregate must be boolean")
        seed = changes.get("seed", 1)
        if type(seed) is not int:
            raise ValueError("seed must be an integer")
        return Medium(current["signal"], current["jitter"], current["loss"],
                      current["latency_ms"], current["rate_index"],
                      current["aggregate"], seed)

    def signal(self):
        if not self.jitter_db:
            return self.signal_dbm
        return max(-110, min(0, self.signal_dbm + self.random.randint(-self.jitter_db,
                                                                        self.jitter_db)))

    def dropped(self):
        return self.loss > 0 and self.random.random() < self.loss

    def rate(self, rates=b""):
        if self.rate_index is not None:
            return self.rate_index
        return rates[0] if rates and rates[0] != 0xff else 0


def u32(value):
    return struct.pack("=I", value & 0xFFFFFFFF)


def number(data):
    if len(data) != 4:
        raise ValueError("expected a netlink u32")
    return struct.unpack("=I", data)[0]


def attributes(values):
    result = bytearray()
    for kind, value in values.items():
        length = 4 + len(value)
        result.extend(struct.pack("=HH", length, kind) + value)
        result.extend(bytes((-length) % 4))
    return bytes(result)


def parse_attributes(data):
    result = {}
    while data:
        if len(data) < 4:
            raise ValueError("truncated netlink attribute")
        length, kind = struct.unpack_from("=HH", data)
        if length < 4 or length > len(data):
            raise ValueError("invalid netlink attribute length")
        kind &= 0x3FFF
        if kind in result:
            raise ValueError("duplicate netlink attribute")
        result[kind] = data[4:length]
        data = data[(length + 3) & ~3:]
    return result


def parse_messages(data):
    while data:
        if len(data) < 16:
            raise ValueError("truncated netlink header")
        length, kind, flags, sequence, pid = struct.unpack_from("=IHHII", data)
        if length < 16 or length > len(data):
            raise ValueError("invalid netlink message length")
        yield kind, sequence, data[16:length]
        data = data[(length + 3) & ~3:]


class Netlink:
    """One generic-netlink socket; requests and unsolicited frames share it."""

    def __init__(self):
        self.socket = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 16)
        self.socket.bind((0, 0))
        self.socket.settimeout(3)
        self.sequence = 0
        self.family = 0x10  # Generic-netlink controller, CTRL_CMD_GETFAMILY.
        try:
            seq = self.send(3, {2: b"MAC80211_HWSIM\0"})
            resolved = None
            acknowledged = False
            while not acknowledged or resolved is None:
                for kind, reply_seq, payload in self.receive():
                    if reply_seq != seq:
                        continue
                    if kind == 2:
                        self.check_ack(payload)
                        acknowledged = True
                    elif kind == 0x10:
                        attrs = parse_attributes(payload[4:])
                        resolved = struct.unpack("=H", attrs[1])[0]
            self.family = resolved
        except BaseException:
            self.socket.close()
            raise

    def send(self, command, attrs):
        self.sequence += 1
        body = struct.pack("=BBH", command, 1, 0) + attributes(attrs)
        header = struct.pack("=IHHII", len(body) + 16, self.family, 5,
                             self.sequence, self.socket.getsockname()[0])
        self.socket.sendto(header + body, (0, 0))
        return self.sequence

    def receive(self):
        data, _, flags, sender = self.socket.recvmsg(65536)
        if flags & socket.MSG_TRUNC or sender[0] != 0:
            raise ValueError("truncated or non-kernel netlink datagram")
        return list(parse_messages(data))

    @staticmethod
    def check_ack(payload):
        if len(payload) < 4:
            raise ValueError("truncated netlink acknowledgement")
        error = struct.unpack_from("=i", payload)[0]
        if error:
            raise OSError(-error, os.strerror(-error))

    def register(self):
        sequence = self.send(REGISTER, {})
        early = []
        acknowledged = False
        while not acknowledged:
            for kind, seq, payload in self.receive():
                if kind == 2 and seq == sequence:
                    self.check_ack(payload)
                    acknowledged = True
                else:
                    early.append((kind, seq, payload))
                    if len(early) > MAX_PENDING:
                        raise ValueError("too many frames during registration")
        return early


class Bridge:
    """Bounded routing and completion tracking, independent of socket I/O."""

    def __init__(self, kernel, radios, emit, now=time.monotonic, medium=None, counters=None):
        self.kernel, self.radios, self.emit, self.now = kernel, radios, emit, now
        self.medium = medium or Medium()
        self.pending = {}
        self.injecting = {}
        self.delayed = []
        self.scheduled = {}
        self.next_token = 0
        # A daemon keeps one counter dict per instance so the totals survive a
        # guest reconnecting to the same radios.
        self.counters = counters if counters is not None else {
            "kernel_frames": 0, "frontend_frames": 0, "dropped": 0, "injected": 0, "rejected": 0}

    def control(self, request):
        if not isinstance(request, dict) or request.get("version") != 1:
            raise ValueError("expected control protocol version 1")
        operation = request.get("type")
        if operation == "stats":
            return {"version": 1, "type": "stats", "medium": self.medium.settings(),
                    "counters": dict(self.counters), "pending": len(self.pending),
                    "scheduled": len(self.scheduled), "injecting": len(self.injecting)}
        if operation == "configure":
            self.medium = self.medium.reconfigured(request.get("settings"))
            return {"version": 1, "type": "configured", "medium": self.medium.settings()}
        raise ValueError("unsupported control operation")

    def status(self, attrs, acknowledged, signal=None):
        flags = number(attrs[FLAGS]) & ~ACK
        if acknowledged and not flags & NO_ACK:
            flags |= ACK
        self.kernel.send(TX_INFO, {
            TRANSMITTER: attrs[TRANSMITTER], COOKIE: attrs[COOKIE],
            FLAGS: u32(flags), SIGNAL: u32(self.medium.signal() if signal is None else signal),
            RATES: attrs[RATES],
        })

    def from_kernel(self, kind, seq, payload):
        if kind == 2:
            injection = self.injecting.pop(seq, None)
            try:
                Netlink.check_ack(payload)
            except OSError as error:
                if injection is None:
                    raise
                self.counters["rejected"] += 1
                self.emit({"type": "injected", "id": injection[0],
                           "accepted": False, "error": str(error)})
            else:
                if injection is not None:
                    # Kernel accepted RX injection, not an over-the-air ACK.
                    self.emit({"type": "injected", "id": injection[0], "accepted": True})
            return
        if kind != self.kernel.family or len(payload) < 4 or payload[0] != FRAME:
            return
        self.counters["kernel_frames"] += 1
        attrs = parse_attributes(payload[4:])
        for field, length in [(TRANSMITTER, 6), (FLAGS, 4), (COOKIE, 8), (RATES, 8)]:
            if len(attrs.get(field, b"")) != length:
                raise ValueError("malformed hwsim TX event")
        radio = next((name for name, mac in self.radios.items()
                      if mac == attrs[TRANSMITTER]), None)
        frame = attrs.get(DATA, b"")
        if radio is None or not 24 <= len(frame) <= 2304 or len(self.pending) >= MAX_PENDING:
            self.status(attrs, False)
            return
        signal = self.medium.signal()
        if self.medium.dropped():
            self.counters["dropped"] += 1
            self.status(attrs, False, signal)
            return
        frequency = number(attrs.get(FREQ, b""))
        self.next_token += 1
        token = self.next_token
        due = self.now() + self.medium.latency
        self.pending[token] = (due + 1, attrs, signal)
        message = {"type": "rx", "id": token, "radio": radio, "frequency": frequency,
                   "frame": frame.hex(), "flags": number(attrs[FLAGS]), "signal": signal,
                   "rate": self.medium.rate(attrs[RATES]),
                   "aggregate": self.medium.aggregate and frame[0] & 0x8c == 0x88}
        if due <= self.now():
            self.emit(message)
        else:
            heapq.heappush(self.delayed, (due, token, message))

    def from_frontend(self, message):
        if not isinstance(message, dict) or message.get("version") != 1:
            raise ValueError("expected protocol version 1")
        token = message.get("id")
        if type(token) is not int or not 0 <= token < 2**63:
            raise ValueError("invalid frame id")
        if message.get("type") == "rx-status":
            acknowledged = message.get("acked")
            if type(acknowledged) is not bool:
                raise ValueError("acked must be boolean")
            if token not in self.pending:
                raise ValueError("unknown or expired RX id")
            _, attrs, signal = self.pending.pop(token)
            self.status(attrs, acknowledged, signal)
            return
        if message.get("type") != "tx":
            raise ValueError("unsupported frontend operation")
        self.counters["frontend_frames"] += 1
        radio = message.get("radio")
        if not isinstance(radio, str) or radio not in self.radios:
            raise ValueError("unmapped radio")
        frequency = message.get("frequency")
        if type(frequency) is not int or not 2300 <= frequency <= 7125:
            raise ValueError("invalid frequency in MHz")
        raw = message.get("frame")
        if not isinstance(raw, str) or not 48 <= len(raw) <= 4608:
            raise ValueError("expected raw 802.11 frame, without radiotap/FCS")
        frame = bytes.fromhex(raw)
        if not 24 <= len(frame) <= 2304 or frame[0] & 3:
            raise ValueError("invalid 802.11 frame")
        rate = message.get("rate", self.medium.rate())
        aggregate = message.get("aggregate", False)
        if type(rate) is not int or not 0 <= rate < 32 or type(aggregate) is not bool:
            raise ValueError("invalid rate or aggregate metadata")
        if (len(self.injecting) + len(self.scheduled) >= MAX_PENDING
                or any(v[0] == token for v in self.injecting.values()) or token in self.scheduled):
            raise ValueError("injection queue full or duplicate id")
        if self.medium.dropped():
            self.counters["dropped"] += 1
            self.counters["rejected"] += 1
            self.emit({"type": "injected", "id": token, "accepted": False,
                       "error": "simulated medium loss"})
            return
        due = self.now() + self.medium.latency
        injection = (radio, frequency, frame, rate)
        if due > self.now():
            self.scheduled[token] = (due, injection)
            return
        self.inject(token, injection)

    def inject(self, token, injection):
        radio, frequency, frame, rate = injection
        seq = self.kernel.send(FRAME, {
            RECEIVER: self.radios[radio], DATA: frame, FREQ: u32(frequency),
            RX_RATE: u32(rate), SIGNAL: u32(self.medium.signal()),
        })
        self.injecting[seq] = (token, self.now() + 3)
        self.counters["injected"] += 1

    def expire(self):
        while self.delayed and self.delayed[0][0] <= self.now():
            _, token, message = heapq.heappop(self.delayed)
            if token in self.pending:
                self.emit(message)
        for token, (due, injection) in list(self.scheduled.items()):
            if self.now() >= due:
                del self.scheduled[token]
                self.inject(token, injection)
        for token, (deadline, attrs, signal) in list(self.pending.items()):
            if self.now() >= deadline:
                del self.pending[token]
                self.status(attrs, False, signal)
        for seq, (token, deadline) in list(self.injecting.items()):
            if self.now() >= deadline:
                del self.injecting[seq]
                self.counters["rejected"] += 1
                self.emit({"type": "injected", "id": token, "accepted": False,
                           "error": "kernel acknowledgement timed out"})

    def close(self):
        for _, attrs, signal in self.pending.values():
            self.status(attrs, False, signal)
        self.pending.clear()
        for token in self.scheduled:
            self.emit({"type": "injected", "id": token, "accepted": False,
                       "error": "frontend disconnected before delayed injection"})
        self.scheduled.clear()


class Instance:
    """One console session: its own frame socket, radio pair and medium.

    Several instances share this process, its netlink socket and its hwsim
    namespace; each keeps a separate medium profile and counter set so one
    guest's RF settings never move another's.
    """

    def __init__(self, name, path, radios, medium=None):
        self.name = name
        self.path = Path(path)
        self.radios = radios
        self.medium = medium or Medium()
        self.counters = {"kernel_frames": 0, "frontend_frames": 0,
                         "dropped": 0, "injected": 0, "rejected": 0}
        self.connections = 0
        self.server = None
        self.peer = None
        self.bridge = None
        self.output = deque()
        self.incoming = bytearray()

    @property
    def connected(self):
        return self.peer is not None

    def listen(self):
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        old_umask = os.umask(0o177)
        try:
            self.server.bind(str(self.path))
        finally:
            os.umask(old_umask)
        self.server.listen(1)
        self.server.setblocking(False)

    def emit(self, message):
        if len(self.output) >= MAX_PENDING:
            raise ValueError("frontend output queue full")
        self.output.append((json.dumps({"version": 1, **message}) + "\n").encode())

    def attach(self, kernel, peer):
        self.peer = peer
        self.peer.setblocking(False)
        self.incoming = bytearray()
        self.output = deque()
        self.connections += 1
        self.bridge = Bridge(kernel, self.radios, self.emit,
                             medium=self.medium, counters=self.counters)
        self.emit({"type": "ready", "radios": list(self.radios),
                   "frontend": "raw-80211", "instance": self.name})

    def detach(self):
        if self.bridge is not None:
            try:
                self.bridge.close()
            except OSError:
                pass
        self.bridge = None
        if self.peer is not None:
            self.peer.close()
        self.peer = None
        self.output = deque()
        self.incoming = bytearray()

    def owns(self, mac):
        return mac in self.radios.values()

    def status(self):
        return {"instance": self.name, "socket": str(self.path),
                "radios": {name: mac.hex(":") for name, mac in self.radios.items()},
                "connected": self.connected, "connections": self.connections}

    def control(self, request):
        """Answer `stats` and `configure` whether or not a guest is attached."""
        operation = request.get("type")
        if operation == "stats":
            bridge = self.bridge
            return {"version": 1, "type": "stats", **self.status(),
                    "medium": self.medium.settings(), "counters": dict(self.counters),
                    "pending": len(bridge.pending) if bridge else 0,
                    "scheduled": len(bridge.scheduled) if bridge else 0,
                    "injecting": len(bridge.injecting) if bridge else 0}
        if operation == "configure":
            self.medium = self.medium.reconfigured(request.get("settings"))
            if self.bridge is not None:
                self.bridge.medium = self.medium
            return {"version": 1, "type": "configured", **self.status(),
                    "medium": self.medium.settings()}
        raise ValueError("unsupported control operation")

    def close(self):
        self.detach()
        if self.server is not None:
            self.server.close()
            self.server = None
        self.path.unlink(missing_ok=True)


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


def serve_instances(kernel, instances, control=None, stop=None):
    """Run every instance of a multi-session daemon on one netlink socket.

    `stop` is an optional predicate polled between passes so a supervisor (or a
    test) can wind the daemon down without closing its sockets underneath it.
    """
    # Frames from radios nobody claims still need a completion, or the kernel
    # waits for a status that never arrives.
    spare = Bridge(kernel, {}, lambda message: None)

    def dispatch(kind, sequence, payload):
        if kind == 2:
            owner = next((instance for instance in instances if instance.bridge is not None
                          and sequence in instance.bridge.injecting), None)
            if owner is not None:
                owner.bridge.from_kernel(kind, sequence, payload)
            return
        if kind != kernel.family or len(payload) < 4 or payload[0] != FRAME:
            return
        attrs = parse_attributes(payload[4:])
        transmitter = attrs.get(TRANSMITTER)
        owner = next((instance for instance in instances
                      if instance.bridge is not None and instance.owns(transmitter)), None)
        (owner.bridge if owner is not None else spare).from_kernel(kind, sequence, payload)

    with selectors.DefaultSelector() as selector:
        selector.register(kernel.socket, selectors.EVENT_READ)
        if control is not None:
            control.setblocking(False)
            selector.register(control, selectors.EVENT_READ)
        for instance in instances:
            selector.register(instance.server, selectors.EVENT_READ, ("server", instance))
        try:
            for event in kernel.register():
                dispatch(*event)
            print(f"serving {len(instances)} instance(s): "
                  + ", ".join(f"{instance.name}={instance.path}" for instance in instances),
                  file=sys.stderr, flush=True)
            while stop is None or not stop():
                for instance in instances:
                    if instance.peer is not None:
                        # selectors.modify() clears the key data unless it is
                        # passed again, and the loop routes events by that data.
                        selector.modify(instance.peer, selectors.EVENT_READ |
                                        (selectors.EVENT_WRITE if instance.output else 0),
                                        ("peer", instance))
                for key, mask in selector.select(0.1):
                    if key.fileobj is kernel.socket:
                        for event in kernel.receive():
                            dispatch(*event)
                    elif control is not None and key.fileobj is control:
                        raw, address = control.recvfrom(MAX_LINE)
                        try:
                            response = control_request(instances, json.loads(raw))
                        except (ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
                            response = {"version": 1, "type": "error", "error": str(error)}
                        if address:
                            control.sendto(json.dumps(response).encode(), address)
                    elif isinstance(key.data, tuple) and key.data[0] == "server":
                        instance = key.data[1]
                        peer, _ = instance.server.accept()
                        if instance.connected:
                            # One guest owns a radio pair; refuse a second.
                            peer.close()
                            continue
                        instance.attach(kernel, peer)
                        selector.register(peer, selectors.EVENT_READ, ("peer", instance))
                    else:
                        instance = key.data[1]
                        if not _pump(instance, mask):
                            selector.unregister(instance.peer)
                            instance.detach()
                for instance in instances:
                    if instance.bridge is not None:
                        instance.bridge.expire()
        finally:
            for instance in instances:
                instance.close()


def _pump(instance, mask):
    """Move one instance's bytes; False once the frontend has disconnected."""
    if mask & selectors.EVENT_READ:
        chunk = instance.peer.recv(4096)
        if not chunk:
            return False
        instance.incoming.extend(chunk)
        if len(instance.incoming) > MAX_LINE:
            raise ValueError("frontend message too large")
        while b"\n" in instance.incoming:
            line, _, remainder = instance.incoming.partition(b"\n")
            instance.incoming = bytearray(remainder)
            instance.bridge.from_frontend(json.loads(line))
    if mask & selectors.EVENT_WRITE and instance.output:
        sent = instance.peer.send(instance.output[0])
        instance.output[0] = instance.output[0][sent:]
        if not instance.output[0]:
            instance.output.popleft()
    return True


def load_instances(path):
    """Read the daemon's instance table: name, socket path and radio MACs."""
    document = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(document, list) or not document:
        raise ValueError("instance table must be a nonempty list")
    instances, paths, macs = [], set(), set()
    for entry in document:
        if not isinstance(entry, dict) or set(entry) - {"name", "socket", "radios"}:
            raise ValueError("each instance needs name, socket and radios")
        name, socket_path, radios = entry.get("name"), entry.get("socket"), entry.get("radios")
        if not isinstance(name, str) or not name or any(item.name == name for item in instances):
            raise ValueError("instance names must be unique and nonempty")
        if not isinstance(socket_path, str) or not socket_path or socket_path in paths:
            raise ValueError("instance socket paths must be unique and nonempty")
        if not isinstance(radios, dict) or not radios:
            raise ValueError("each instance needs at least one radio mapping")
        mapping = {}
        for radio, address in radios.items():
            if not isinstance(radio, str) or not isinstance(address, str):
                raise ValueError("radio mappings are name to MAC address")
            mac = bytes.fromhex(address.replace(":", ""))
            if len(mac) != 6 or mac in macs:
                raise ValueError("radio MAC addresses must be unique six-byte values")
            macs.add(mac)
            mapping[radio] = mac
        paths.add(socket_path)
        instances.append(Instance(name, socket_path, mapping))
    return instances


def serve(kernel, peer, radios, medium=None, control=None):
    output = deque()

    def emit(message):
        if len(output) >= MAX_PENDING:
            raise ValueError("frontend output queue full")
        output.append((json.dumps({"version": 1, **message}) + "\n").encode())

    bridge = Bridge(kernel, radios, emit, medium=medium)
    peer.setblocking(False)
    incoming = bytearray()
    with selectors.DefaultSelector() as selector:
        selector.register(peer, selectors.EVENT_READ)
        selector.register(kernel.socket, selectors.EVENT_READ)
        if control is not None:
            control.setblocking(False)
            selector.register(control, selectors.EVENT_READ)
        try:
            for event in kernel.register():
                bridge.from_kernel(*event)
            emit({"type": "ready", "radios": list(radios), "frontend": "raw-80211"})
            while True:
                selector.modify(peer, selectors.EVENT_READ |
                                (selectors.EVENT_WRITE if output else 0))
                for key, mask in selector.select(0.1):
                    if key.fileobj is kernel.socket:
                        for event in kernel.receive():
                            bridge.from_kernel(*event)
                    elif key.fileobj is control:
                        raw, address = control.recvfrom(MAX_LINE)
                        try:
                            response = bridge.control(json.loads(raw))
                        except (ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
                            response = {"version": 1, "type": "error", "error": str(error)}
                        if address:
                            control.sendto(json.dumps(response).encode(), address)
                    else:
                        if mask & selectors.EVENT_READ:
                            chunk = peer.recv(4096)
                            if not chunk:
                                return
                            incoming.extend(chunk)
                            if len(incoming) > MAX_LINE:
                                raise ValueError("frontend message too large")
                            while b"\n" in incoming:
                                line, _, remainder = incoming.partition(b"\n")
                                incoming = bytearray(remainder)
                                bridge.from_frontend(json.loads(line))
                        if mask & selectors.EVENT_WRITE and output:
                            sent = peer.send(output[0])
                            output[0] = output[0][sent:]
                            if not output[0]:
                                output.popleft()
                bridge.expire()
        finally:
            bridge.close()


def main():
    def stop(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, stop)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", action="store_true", help="read-only hwsim family lookup")
    parser.add_argument("--socket", type=Path, help="new private Unix socket; never overwrites a path")
    parser.add_argument("--instances", type=Path,
                        help="JSON instance table: serve several console sessions from this "
                             "daemon, each with its own socket and radio pair")
    parser.add_argument("--control", type=Path,
                        help="optional private Unix datagram control socket")
    parser.add_argument("--radio", action="append", default=[], metavar="NAME=HWSIM_RADIO_MAC")
    parser.add_argument("--own-medium", action="store_true",
                        help="explicitly authorize taking over this namespace's hwsim medium")
    parser.add_argument("--signal", type=int, default=-40, help="mean RSSI in dBm")
    parser.add_argument("--signal-jitter", type=int, default=0, help="uniform RSSI jitter in dB")
    parser.add_argument("--loss", type=float, default=0, help="frame loss probability, zero to one")
    parser.add_argument("--latency-ms", type=int, default=0, help="one-way delivery delay")
    parser.add_argument("--rate-index", type=int, help="force legacy hwsim rate index")
    parser.add_argument("--aggregate", action="store_true", help="mark QoS data as aggregated")
    parser.add_argument("--seed", type=int, default=1, help="deterministic RF random seed")
    args = parser.parse_args()
    radios = {}
    for mapping in args.radio:
        name, separator, address = mapping.partition("=")
        mac = bytes.fromhex(address.replace(":", ""))
        if not separator or not name or name in radios or len(mac) != 6 or mac in radios.values():
            parser.error("radio mappings must have unique names and six-byte MAC addresses")
        radios[name] = mac
    if args.instances and (args.socket or radios):
        parser.error("--instances carries its own sockets and radios")
    if not args.probe and not args.own_medium:
        parser.error("serving requires --own-medium")
    if not args.probe and not args.instances and (not args.socket or not radios):
        parser.error("serving requires --socket and --radio, or --instances")
    kernel = Netlink()
    try:
        if args.probe:
            print(f"MAC80211_HWSIM family {kernel.family}; medium not registered")
            return
        if args.instances:
            instances = load_instances(args.instances)
            for instance in instances:
                instance.listen()
            with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control:
                if args.control:
                    old_umask = os.umask(0o177)
                    try:
                        control.bind(str(args.control))
                    finally:
                        os.umask(old_umask)
                try:
                    serve_instances(kernel, instances, control if args.control else None)
                finally:
                    if args.control:
                        args.control.unlink(missing_ok=True)
            return
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
            old_umask = os.umask(0o177)
            try:
                server.bind(str(args.socket))
            finally:
                os.umask(old_umask)
            try:
                server.listen(1)
                print(f"waiting for raw-802.11 frontend at {args.socket}", file=sys.stderr)
                peer, _ = server.accept()
                with peer, socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control:
                    if args.control:
                        old_umask = os.umask(0o177)
                        try:
                            control.bind(str(args.control))
                        finally:
                            os.umask(old_umask)
                    medium = Medium(args.signal, args.signal_jitter, args.loss, args.latency_ms,
                                    args.rate_index, args.aggregate, args.seed)
                    serve(kernel, peer, radios, medium, control if args.control else None)
            finally:
                args.socket.unlink()
                if args.control:
                    args.control.unlink(missing_ok=True)
    finally:
        kernel.socket.close()


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, struct.error) as error:
        sys.exit(f"hwsim adapter: {error}")
