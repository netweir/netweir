"""Redis, as far as netweir's shared queue uses it, in memory and over the
real protocol, so the queue's tests run where there's no redis-server.

Every command, and every MULTI ... EXEC as a whole, runs under one lock,
as Redis runs them one at a time. The scripts netweir sends are known by
their first line and done here as their Lua does them.

Two faults stand in for a network that fails: ``lose_reply(marker)``
applies the next command (or transaction) with ``marker`` among its
arguments and then drops the connection instead of answering, and
``hang`` stops answering anything at all.
"""

from __future__ import annotations

import contextlib
import socket
import socketserver
import threading
import time


class FakeRedis:
    def __init__(self):
        self.lock = threading.Lock()
        #: key -> {member: score}
        self.zsets: dict[bytes, dict[bytes, float]] = {}
        self.sets: dict[bytes, set[bytes]] = {}
        self.strings: dict[bytes, bytes] = {}
        #: Markers whose next command's reply is lost, once each.
        self.losing: list[bytes] = []
        self.hang = False
        #: When TIME was asked, which a heartbeat does each time.
        self.times: list[float] = []
        self.last_time = 0.0
        #: Scripts run, by their first line.
        self.scripts: dict[str, int] = {}
        #: Seconds each push script's answer is held back, as a slow or
        #: distant server would.
        self.push_delay = 0.0
        #: Delays for the next pushes, one each, before ``push_delay``.
        self.push_delays: list[float] = []
        self.connections: set = set()
        fake = self

        class Handler(socketserver.StreamRequestHandler):
            def setup(self):
                super().setup()
                fake.connections.add(self.request)

            def handle(self):
                queued: list[list[bytes]] | None = None
                while True:
                    try:
                        command = _read(self.rfile)
                    except (ConnectionError, ValueError):
                        return
                    if command is None:
                        return
                    while fake.hang:
                        time.sleep(0.05)
                    name = command[0].upper()
                    applied = [command]
                    if name == b"MULTI":
                        queued = []
                        reply: object = _Status("OK")
                    elif name == b"EXEC":
                        with fake.lock:
                            reply = [fake.run(c) for c in queued or []]
                        applied, queued = queued or [], None
                    elif queued is not None:
                        queued.append(command)
                        reply = _Status("QUEUED")
                        applied = []
                    else:
                        with fake.lock:
                            reply = fake.run(command)
                    replies = reply if name == b"EXEC" else [reply]
                    if fake.lost(applied, replies):
                        return  # applied, and the reply never sent
                    if any(b"-- netweir push" in arg for c in applied for arg in c):
                        time.sleep(fake.push_delays.pop(0) if fake.push_delays else fake.push_delay)
                    self.wfile.write(_encode(reply))

        self.server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"redis://127.0.0.1:{self.server.server_address[1]}/0"

    def close(self) -> None:
        self.hang = False
        self.server.shutdown()
        self.server.server_close()
        for filler in getattr(self, "fillers", []):
            filler.close()

    def black_hole(self) -> None:
        """Drops every connection, and from then on takes new ones (the
        port stays open) without ever answering on them: the server a
        client reconnects to in vain."""
        self.server.shutdown()  # no more accepting; the socket stays open
        for connection in list(self.connections):
            with contextlib.suppress(OSError):
                connection.shutdown(socket.SHUT_RDWR)
        # A full backlog: from here on a connection attempt isn't even
        # answered, so it waits out its own timeout, as one to a host that
        # drops packets does.
        self.fillers = []
        for _ in range(64):
            filler = socket.socket()
            filler.setblocking(False)
            with contextlib.suppress(OSError):
                filler.connect(self.server.server_address)
            self.fillers.append(filler)

    def lose_reply(self, marker: bytes, did_something: bool = False) -> None:
        """Loses the reply of the next command with ``marker`` among its
        arguments; with ``did_something``, only of one that answered with
        a count above 0 (a ZRANGESTORE that took a request, say)."""
        with self.lock:
            self.losing.append((marker, did_something))

    def lost(self, applied: list[list[bytes]], replies: list) -> bool:
        with self.lock:
            for marker, did_something in self.losing:
                for command, reply in zip(applied, replies, strict=False):
                    if not any(marker in arg for arg in command):
                        continue
                    if did_something and not (isinstance(reply, int) and reply > 0):
                        continue
                    self.losing.remove((marker, did_something))
                    return True
        return False

    def zset(self, key: bytes) -> dict[bytes, float]:
        return self.zsets.setdefault(key, {})

    def ordered(self, key: bytes) -> list[tuple[bytes, float]]:
        return sorted(self.zset(key).items(), key=lambda kv: (kv[1], kv[0]))

    def store(self, key: bytes, members: dict[bytes, float]) -> int:
        self.zsets[key] = dict(members)
        return len(members)

    def run(self, c: list[bytes]) -> object:
        name, a = c[0].upper().decode(), c[1:]
        if name in ("CLIENT", "SELECT", "AUTH"):
            return _Status("OK")
        if name == "PING":
            return _Status("PONG")
        if name == "TIME":
            now = self.last_time = time.time()
            self.times.append(now)
            return [str(int(now)).encode(), str(int(now % 1 * 1_000_000)).encode()]
        if name == "EVAL":
            n = int(a[1])
            return self.script(a[0].decode().splitlines()[0], a[2 : 2 + n], a[2 + n :])
        if name == "ZADD":
            key, i, flags = a[0], 1, set()
            while a[i].upper() in (b"NX", b"XX", b"LT"):
                flags.add(a[i].upper())
                i += 1
            z, added = self.zset(key), 0
            for score, member in zip(a[i::2], a[i + 1 :: 2], strict=True):
                there = member in z
                if (b"NX" in flags and there) or (b"XX" in flags and not there):
                    continue
                if b"LT" in flags and there and float(score) >= z[member]:
                    continue
                added += not there
                z[member] = float(score)
            return added
        if name == "ZRANGE":
            members = self.ordered(a[0])[_ranks(len(self.zset(a[0])), a[1], a[2])]
            return [m for m, _ in members]
        if name == "ZRANGESTORE":
            members = self.ordered(a[1])[_ranks(len(self.zset(a[1])), a[2], a[3])]
            return self.store(a[0], dict(members))
        if name == "ZREMRANGEBYRANK":
            members = self.ordered(a[0])[_ranks(len(self.zset(a[0])), a[1], a[2])]
            for m, _ in members:
                del self.zset(a[0])[m]
            return len(members)
        if name == "ZUNIONSTORE":
            n = int(a[1])
            assert [x.upper() for x in a[2 + n :]] == [b"AGGREGATE", b"MIN"]
            union: dict[bytes, float] = {}
            for key in a[2 : 2 + n]:
                for m, s in self.zset(key).items():
                    union[m] = min(s, union.get(m, s))
            return self.store(a[0], union)
        if name == "ZREM":
            z = self.zset(a[0])
            return sum(z.pop(m, None) is not None for m in a[1:])
        if name == "ZCARD":
            return len(self.zset(a[0]))
        if name == "ZRANGEBYSCORE":
            lo, hi = _score(a[1]), _score(a[2])
            return [m for m, s in self.ordered(a[0]) if lo <= s <= hi]
        if name == "DEL":
            gone = 0
            for key in a:
                had = bool(self.zsets.pop(key, None)) | bool(self.sets.pop(key, None))
                gone += had | (self.strings.pop(key, None) is not None)
            return gone
        if name == "SADD":
            s = self.sets.setdefault(a[0], set())
            added = len(set(a[1:]) - s)
            s.update(a[1:])
            return added
        if name == "SMISMEMBER":
            s = self.sets.get(a[0], set())
            return [int(m in s) for m in a[1:]]
        if name == "SCARD":
            return len(self.sets.get(a[0], set()))
        if name == "INCRBY":
            value = int(self.strings.get(a[0], b"0")) + int(a[1])
            self.strings[a[0]] = str(value).encode()
            return value
        if name == "GET":
            return self.strings.get(a[0])
        if name == "SCAN":
            prefix = a[2].replace(b"\\", b"").removesuffix(b"*")
            keys = [k for k, v in self.zsets.items() if v]
            keys += [k for k, v in self.sets.items() if v] + list(self.strings)
            return [b"0", [k for k in keys if k.startswith(prefix)]]
        return _Error(f"ERR the fake doesn't know {name}")

    def script(self, first_line: str, keys: list[bytes], argv: list[bytes]) -> object:
        self.scripts[first_line] = self.scripts.get(first_line, 0) + 1
        if first_line == "-- netweir push":
            out, queued = [], 0
            for fp, score, member, skip in zip(*[iter(argv)] * 4, strict=True):
                new = self.run([b"SADD", keys[0], fp])
                if new or skip == b"1":
                    queued += self.run([b"ZADD", keys[1], b"NX", score, member])
                    out.append(1)
                else:
                    # Still waiting from an earlier run of this batch.
                    out.append(int(member in self.zset(keys[1])))
            if queued:
                self.run([b"INCRBY", keys[2], str(queued).encode()])
            return out
        if first_line == "-- netweir reap":
            process, now = argv
            if now:
                deadline = self.zset(keys[0]).get(process)
                if deadline is None or deadline > float(now):
                    return -1
            held = self.zsets.pop(keys[2], {})
            for member, score in held.items():
                self.run([b"ZADD", keys[1], b"LT", str(score).encode(), member])
            self.zset(keys[0]).pop(process, None)
            return len(held)
        if first_line == "-- netweir status":
            processes = list(self.zset(keys[3]))
            held = sum(len(self.zset(argv[0] + p)) for p in processes)
            return [
                len(self.zset(keys[0])),
                len(self.sets.get(keys[1], set())),
                int(self.strings.get(keys[2], b"0")),
                len(processes),
                held,
            ]
        if first_line == "-- netweir unhold":
            n = 0
            for member in argv:
                score = self.zset(keys[0]).pop(member, None)
                if score is not None:
                    self.run([b"ZADD", keys[1], b"LT", str(score).encode(), member])
                    n += 1
            return n
        return _Error(f"ERR the fake doesn't know the script {first_line}")


class _Status(str):
    pass


class _Error(str):
    pass


def _ranks(length: int, start: bytes, stop: bytes) -> slice:
    a, b = int(start), int(stop)
    if a < 0:
        a = max(length + a, 0)
    if b < 0:
        b += length
    return slice(a, max(min(b, length - 1) + 1, a))


def _score(text: bytes) -> float:
    return {b"-inf": float("-inf"), b"+inf": float("inf")}.get(text) or float(text)


def _read(rfile) -> list[bytes] | None:
    line = rfile.readline()
    if not line:
        return None
    if not line.startswith(b"*"):
        raise ValueError("inline commands aren't spoken here")
    args = []
    for _ in range(int(line[1:])):
        size = int(rfile.readline()[1:])
        args.append(rfile.read(size + 2)[:-2])
    return args


def _encode(value: object) -> bytes:
    if isinstance(value, _Error):
        return f"-{value}\r\n".encode()
    if isinstance(value, _Status):
        return f"+{value}\r\n".encode()
    if value is None:
        return b"$-1\r\n"
    if isinstance(value, bool | int):
        return f":{int(value)}\r\n".encode()
    if isinstance(value, bytes):
        return b"$%d\r\n%s\r\n" % (len(value), value)
    if isinstance(value, list):
        return b"*%d\r\n" % len(value) + b"".join(_encode(v) for v in value)
    raise TypeError(value)
