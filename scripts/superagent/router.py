#!/usr/bin/env python3
"""Sets up and talks to the Superagent Router of one CODEX_HOME (default ~/.codex).

Usage:
  python3 -I router.py create      start the Router thread in ~ and register it in pm-threads
  python3 -I router.py pin         pin it first in the command center (/agents)
  python3 -I router.py ask "TEXT"  one Router turn; prints its answer

It talks to that home's app-server daemon socket and uses the daemon's default account.
Standard library only.
"""
import base64
import json
import os
import socket
import struct
import sys
import time

HOME = os.path.expanduser("~")
CODEX_HOME = os.environ.get("CODEX_HOME") or os.path.join(HOME, ".codex")
SOCKET = os.path.join(CODEX_HOME, "app-server-control", "app-server-control.sock")
PM_THREADS = os.path.join(CODEX_HOME, "superagent", "pm-threads")
# The command center's pinned section.
PINNED = "01984de2-8f74-7c91-a3b2-5c5e937cf318"


class Conn:
    """JSON-RPC over the daemon's unix socket (WebSocket framing)."""

    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall((f"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = self.s.recv(4096)
            if not chunk:
                raise SystemExit("closed during handshake")
            buf += chunk
        head, self.pending = buf.split(b"\r\n\r\n", 1)
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise SystemExit(head.decode(errors="replace"))
        self.nid = 0
        self.backlog = []

    def _frame(self, op, data):
        hdr = bytearray([0x80 | op])
        n = len(data)
        if n < 126:
            hdr.append(0x80 | n)
        elif n < 65536:
            hdr += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            hdr += bytes([0x80 | 127]) + struct.pack(">Q", n)
        mask = os.urandom(4)
        return bytes(hdr) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data))

    def send(self, obj):
        self.s.sendall(self._frame(0x1, json.dumps(obj).encode()))

    def _exact(self, n):
        while len(self.pending) < n:
            chunk = self.s.recv(65536)
            if not chunk:
                raise SystemExit("socket closed")
            self.pending += chunk
        out, self.pending = self.pending[:n], self.pending[n:]
        return out

    def recv(self, timeout=None):
        self.s.settimeout(timeout)
        msg = b""
        while True:
            try:
                b1, b2 = self._exact(2)
            except socket.timeout:
                return None
            self.s.settimeout(None)
            op, n = b1 & 0x0F, b2 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._exact(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._exact(8))[0]
            mask = self._exact(4) if b2 & 0x80 else None
            payload = self._exact(n)
            if mask:
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            if op == 0x9:
                self.s.sendall(self._frame(0xA, payload))
                continue
            if op == 0x8:
                raise SystemExit("server closed the connection")
            msg += payload
            if b1 & 0x80:
                return json.loads(msg)

    def request(self, method, params, timeout=120):
        self.nid += 1
        my = self.nid
        self.send({"id": my, "method": method, "params": params})
        end = time.time() + timeout
        while time.time() < end:
            m = self.recv(timeout=max(0.1, end - time.time()))
            if m is None:
                break
            if m.get("id") == my and "method" not in m:
                if "error" in m:
                    raise RuntimeError(f"{method}: {m['error']}")
                return m["result"]
            if "method" in m and "id" in m:  # a server -> client request: decline it
                self.send({"id": m["id"], "error": {"code": -32000, "message": "declined by router.py"}})
            self.backlog.append(m)
        raise TimeoutError(method)


def connect():
    conn = Conn(SOCKET)
    # The TUI's client name: the daemon takes its process originator from the first initialize.
    conn.request("initialize", {"clientInfo": {"name": "codex-tui", "version": "0.166.0"},
                                "capabilities": {"experimentalApi": True}}, timeout=30)
    conn.send({"method": "initialized"})
    return conn


def registered():
    if not os.path.exists(PM_THREADS):
        return []
    with open(PM_THREADS) as f:
        return [line.split()[0] for line in f if line.strip() and not line.startswith("#")]


def router():
    threads = registered()
    if not threads:
        sys.exit(f"no Router registered in {PM_THREADS}; run: router.py create")
    return threads[0]


def text(value):
    return [{"type": "text", "text": value, "textElements": []}]


def create():
    if registered():
        sys.exit(f"a Router is registered already: {registered()[0]}")
    conn = connect()
    thread = conn.request("thread/start", {
        "cwd": HOME, "sandbox": "read-only", "approvalPolicy": "never",
        "config": {"model_reasoning_effort": "max"},
    }, timeout=120)["thread"]
    conn.request("thread/name/set", {"threadId": thread["id"], "name": "Router"}, timeout=60)
    os.makedirs(os.path.dirname(PM_THREADS), exist_ok=True)
    with open(PM_THREADS, "a") as f:
        f.write(f"# Router, created {time.strftime('%Y-%m-%d %H:%M')} by router.py\n")
        f.write(thread["id"] + "\n")
    print(json.dumps({"router": thread["id"]}))


def pinned_threads(conn):
    return conn.request("thread/list", {"limit": 100, "sectionId": PINNED, "sortKey": "section_position",
                                        "archived": False}, timeout=60).get("data", [])


def pin():
    thread = router()
    conn = connect()
    first = next((t["id"] for t in pinned_threads(conn) if t["id"] != thread), None)
    conn.request("thread/section/move", {"threadId": thread, "sectionId": PINNED,
                                         "beforeThreadId": first}, timeout=60)
    print(json.dumps({"router": thread, "pinned_in_order": [t["id"] for t in pinned_threads(conn)]}))


def ask(words):
    thread = router()
    conn = connect()
    # A thread the daemon unloaded is resumed first; a loaded one answers with its state.
    conn.request("thread/resume", {"threadId": thread}, timeout=120)
    conn.backlog.clear()
    conn.request("turn/start", {"threadId": thread, "input": text(words)}, timeout=120)
    log = list(conn.backlog)
    end = time.time() + 600
    while not any(m.get("method") == "turn/completed" and m.get("params", {}).get("threadId") == thread
                  for m in log):
        msg = conn.recv(timeout=max(0.1, end - time.time()))
        if msg is None:
            sys.exit("the Router's turn did not finish within 10 minutes")
        log.append(msg)
    answer = [m["params"]["item"].get("text", "") for m in log
              if m.get("method") == "item/completed" and m.get("params", {}).get("threadId") == thread
              and m["params"].get("item", {}).get("type") == "agentMessage"]
    print(json.dumps({"router": thread, "answer": "\n".join(answer)}, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    commands = {"create": create, "pin": pin, "ask": lambda: ask(sys.argv[2])}
    if len(sys.argv) < 2 or sys.argv[1] not in commands or (sys.argv[1] == "ask" and len(sys.argv) < 3):
        sys.exit(__doc__)
    commands[sys.argv[1]]()
