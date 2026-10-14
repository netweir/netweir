"""Windows: does SO_RANDOMIZE_PORT make loopback connects fail with
WSAENOBUFS once ports sit in TIME_WAIT, and does Chrome hit it?"""
import asyncio
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import netweir

SO_RANDOMIZE_PORT = 0x3005
WSAENOBUFS = 10055


def sink():
    """A server that reads until the client closes, then closes: the
    client closes first, so its port goes to TIME_WAIT."""
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1024)

    def serve():
        while True:
            c, _ = srv.accept()
            threading.Thread(target=lambda c=c: (c.recv(1), c.close()), daemon=True).start()

    threading.Thread(target=serve, daemon=True).start()
    return srv.getsockname()


def fill_time_wait(addr, n):
    for _ in range(n):
        s = socket.socket()
        s.connect(addr)
        s.close()


def raw(addr, n, randomize):
    """n connects, each reset on close so it leaves no TIME_WAIT."""
    fails = other = 0
    for _ in range(n):
        s = socket.socket()
        s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, b"\x01\x00\x00\x00")
        if randomize:
            s.setsockopt(socket.SOL_SOCKET, SO_RANDOMIZE_PORT, 1)
        try:
            s.connect(addr)
        except OSError as e:
            if getattr(e, "winerror", None) == WSAENOBUFS:
                fails += 1
            else:
                other += 1
        s.close()
    return fails, other


class Page(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"<p>hi</p>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


async def chrome(url, n, args):
    fails = other = 0
    async with netweir.browser(args=args) as b:
        page = await b.new_page()
        for i in range(n):
            try:
                await page.goto(f"{url}/{i}")
            except Exception as e:
                if "ERR_NO_BUFFER_SPACE" in str(e):
                    fails += 1
                else:
                    other += 1
                    print("other:", e)
    return fails, other


def main():
    addr = sink()
    for fill in (0, 4000, 8000):
        if fill:
            t = time.time()
            fill_time_wait(addr, 4000)
            print(f"filled to ~{fill} TIME_WAIT in {time.time() - t:.1f}s", flush=True)
        n = 5000
        print(f"TIME_WAIT~{fill}: raw randomized {raw(addr, n, True)}, raw default {raw(addr, n, False)} (ENOBUFS, other) of {n}", flush=True)
    http = ThreadingHTTPServer(("127.0.0.1", 0), Page)
    threading.Thread(target=http.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{http.server_port}"
    n = 300
    on = asyncio.run(chrome(url, n, []))
    off = asyncio.run(chrome(url, n, ["--disable-features=TcpPortRandomizationWin"]))
    print(f"Chrome, {n} navigations: as is {on}, TcpPortRandomizationWin off {off} (ENOBUFS, other)", flush=True)


if __name__ == "__main__":
    print(sys.version, sys.getwindowsversion() if sys.platform == "win32" else sys.platform)
    main()
