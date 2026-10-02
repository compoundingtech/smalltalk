#!/usr/bin/env python3
"""A token-free `codex`: the app-server and the TUI that st's Codex driver controls.

st3 runs `codex --version`, `codex app-server generate-json-schema --out DIR`, then
`codex app-server --listen unix://SOCKET` and `codex --remote unix://SOCKET`. The app-server
speaks JSON over WebSocket text frames. This stand-in answers the same few messages the driver
sends, and the TUI starts its thread at once: real Codex takes seconds to draw its first screen,
which hid the race between a bound thread and the daemon's mailbox replay. A fast provider hits
that race every time.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import struct
import sys
import threading
import uuid

import stubmodel

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
HERE = Path(__file__).resolve().parent


def option(argv, name):
    for index, argument in enumerate(argv):
        if argument == name and index + 1 < len(argv):
            return argv[index + 1]
        if argument.startswith(name + "="):
            return argument.split("=", 1)[1]
    return None


def read_exact(stream, count):
    data = b""
    while len(data) < count:
        chunk = stream.recv(count - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data


def read_frame(stream):
    """The next text message, or None once the peer closed."""
    while True:
        head = read_exact(stream, 2)
        opcode, length = head[0] & 0x0F, head[1] & 0x7F
        if length == 126:
            length = struct.unpack(">H", read_exact(stream, 2))[0]
        elif length == 127:
            length = struct.unpack(">Q", read_exact(stream, 8))[0]
        mask = read_exact(stream, 4) if head[1] & 0x80 else None
        payload = read_exact(stream, length)
        if mask:
            payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        if opcode == 8:
            return None
        if opcode == 1:
            return payload.decode()


def frame(text, masked):
    payload = text.encode()
    head = bytes([0x81])
    mask_bit = 0x80 if masked else 0
    if len(payload) < 126:
        head += bytes([mask_bit | len(payload)])
    elif len(payload) < 65536:
        head += bytes([mask_bit | 126]) + struct.pack(">H", len(payload))
    else:
        head += bytes([mask_bit | 127]) + struct.pack(">Q", len(payload))
    if masked:
        mask = os.urandom(4)
        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        return head + mask + payload
    return head + payload


def listen(path):
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    server = socket.socket(socket.AF_UNIX)
    server.bind(path)
    server.listen(8)
    return server


class AppServer:
    def __init__(self):
        self.clients = []
        self.lock = threading.Lock()
        self.thread_id = None
        self.turns = 0
        self.history = []

    def send(self, client, message):
        with self.lock:
            try:
                client.sendall(frame(json.dumps(message), False))
            except OSError:
                pass

    def broadcast(self, message):
        for client in list(self.clients):
            self.send(client, message)

    def serve(self, client):
        request = b""
        while b"\r\n\r\n" not in request:
            chunk = client.recv(4096)
            if not chunk:
                return
            request += chunk
        key = next(line.split(b":", 1)[1].strip() for line in request.split(b"\r\n")
                   if line.lower().startswith(b"sec-websocket-key"))
        accept = base64.b64encode(hashlib.sha1(key + GUID.encode()).digest()).decode()
        client.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                        f"Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").encode())
        self.clients.append(client)
        stubmodel.receipt("app-server-client", clients=len(self.clients))
        try:
            while True:
                text = read_frame(client)
                if text is None:
                    return
                message = json.loads(text)
                stubmodel.receipt("wire", method=message.get("method"), id=message.get("id"),
                                  client=self.clients.index(client))
                self.handle(client, message)
        except (EOFError, OSError, ValueError):
            pass
        except Exception as error:
            stubmodel.receipt("app-server-error", error=repr(error))
        finally:
            stubmodel.receipt("app-server-client-gone", clients=len(self.clients) - 1)
            if client in self.clients:
                self.clients.remove(client)

    def thread(self):
        return {"id": self.thread_id, "status": {"type": "idle"}, "turns": list(self.history)}

    def handle(self, client, message):
        method, ident = message.get("method"), message.get("id")
        if method is None or ident is None:
            return
        params = message.get("params") or {}
        if method == "initialize":
            self.send(client, {"id": ident, "result": {"userAgent": "codex-stub"}})
        elif method == "thread/start":
            self.thread_id = self.thread_id or str(uuid.uuid4())
            self.send(client, {"id": ident, "result": {"thread": self.thread()}})
            self.broadcast({"method": "thread/started", "params": {"thread": self.thread()}})
        elif method == "thread/loaded/list":
            self.send(client, {"id": ident, "result": {"data": [self.thread_id] if self.thread_id else []}})
        elif method == "thread/resume":
            if params.get("threadId") != self.thread_id:
                self.send(client, {"id": ident, "error": {
                    "code": -32600, "message": f"no rollout found for thread id {params.get('threadId')}"}})
            else:
                self.send(client, {"id": ident, "result": {"thread": self.thread()}})
        elif method == "account/read":
            self.send(client, {"id": ident, "result": {"account": {"type": "apiKey"}}})
        elif method == "thread/read":
            self.send(client, {"id": ident, "result": {"thread": self.thread()}})
        elif method == "turn/start":
            self.turns += 1
            turn = f"turn-{self.turns}"
            text = "".join(part.get("text", "") for part in params.get("input", []))
            stubmodel.receipt("turn", text=text)
            self.history.append({"id": turn, "status": "completed", "items": [
                {"type": "userMessage", "id": f"item-{self.turns}",
                 "clientId": params.get("clientUserMessageId"), "content": []}]})
            self.send(client, {"id": ident, "result": {"turn": {"id": turn}}})
            self.broadcast({"method": "item/completed", "params": {
                "threadId": self.thread_id, "turnId": turn,
                "item": {"type": "userMessage", "id": f"item-{self.turns}",
                         "clientId": params.get("clientUserMessageId"), "content": []}}})
            threading.Thread(target=self.finish, args=(turn, text), daemon=True).start()
        else:
            self.send(client, {"id": ident, "result": {}})

    def finish(self, turn, text):
        stubmodel.act(text)
        self.broadcast({"method": "turn/completed", "params": {
            "threadId": self.thread_id, "turn": {"id": turn, "status": "completed"}}})


def app_server(argv):
    if "generate-json-schema" in argv:
        out = Path(option(argv, "--out"))
        for schema in (HERE / "codex-schemas").glob("*.json"):
            (out / schema.name).write_bytes(schema.read_bytes())
        return 0
    path = option(argv, "--listen")
    path = path.removeprefix("unix://")
    server = listen(path)
    state = AppServer()
    def ended(number, _frame):
        stubmodel.receipt("app-server-signal", signal=number)
        os._exit(0)
    for number in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
        signal.signal(number, ended)
    while True:
        client, _ = server.accept()
        threading.Thread(target=state.serve, args=(client,), daemon=True).start()


def tui(argv):
    path = option(argv, "--remote").removeprefix("unix://")
    stream = socket.socket(socket.AF_UNIX)
    stream.connect(path)
    key = base64.b64encode(os.urandom(16)).decode()
    stream.sendall((f"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                    f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
    response = b""
    while b"\r\n\r\n" not in response:
        response += stream.recv(4096)

    def call(ident, method, params):
        stream.sendall(frame(json.dumps({"id": ident, "method": method, "params": params}), True))
        while True:
            text = read_frame(stream)
            if text is None:
                raise EOFError
            if json.loads(text).get("id") == ident:
                return

    call(0, "initialize", {"clientInfo": {"name": "codex-stub", "version": "0"}})
    stream.sendall(frame(json.dumps({"method": "initialized", "params": {}}), True))
    call(1, "thread/start", {"cwd": os.getcwd()})
    stubmodel.receipt("tui-ready")
    signal.signal(signal.SIGTERM, lambda *_: os._exit(0))
    try:
        while read_frame(stream) is not None:
            pass
    except (EOFError, OSError):
        pass
    return 0


def main(argv):
    if "--version" in argv:
        print("codex-cli 0.0.0-stub")
        return 0
    stubmodel.receipt("started", argv=argv)
    if argv[:1] == ["app-server"]:
        return app_server(argv[1:])
    return tui(argv)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
