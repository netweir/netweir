"""Python exiting straight after awaiting one of netweir's futures.

The result of a Rust future reaches Python from a Rust thread. Before the
fix, a program whose last act was such an await could reach its end while
that thread still held Python's GIL, and CPython before 3.14 ended the
thread mid-way: a crash at exit (SIGABRT or SIGSEGV, on Linux), a few times
in a hundred on a busy machine.
"""

import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("content-length", "2")
        self.end_headers()
        self.wfile.write(b"ok")


@pytest.fixture
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}/"
    server.shutdown()
    server.server_close()


LAST_ACT_IS_AN_AWAIT = """
import asyncio, sys
import netweir

async def main():
    async with netweir.Client() as client:
        await client.get(sys.argv[1])

asyncio.run(main())
"""


def test_exiting_straight_after_an_await_is_clean(base):
    failures = []
    for _ in range(40):
        done = subprocess.run(
            [sys.executable, "-c", LAST_ACT_IS_AN_AWAIT, base],
            capture_output=True,
            text=True,
            timeout=60,
        )
        if done.returncode != 0:
            failures.append((done.returncode, done.stderr[-500:]))
    assert failures == []


def run_python(code, *args, timeout=60):
    return subprocess.run(
        [sys.executable, "-c", code, *args], capture_output=True, text=True, timeout=timeout
    )


def test_a_panic_in_rust_raises_instead_of_hanging():
    import asyncio

    from netweir import _native

    async def main():
        return await asyncio.wait_for(_native._test_panic(), 10)

    with pytest.raises(BaseException) as raised:
        asyncio.run(main())
    assert not isinstance(raised.value, asyncio.TimeoutError)
    assert "the test asked for a panic" in str(raised.value)


AN_ATEXIT_HOOK_FROM_BEFORE_THE_IMPORT = """
import atexit, asyncio

def flush():
    from netweir import _native

    async def last_words():
        return await asyncio.wait_for(_native._test_sleep(0.05), 10)

    asyncio.run(last_words())
    print("flushed")

atexit.register(flush)
import netweir
"""


def test_an_atexit_hook_registered_before_the_import_gets_its_results():
    # atexit runs hooks last-registered first, so this one runs after
    # netweir's own.
    done = run_python(AN_ATEXIT_HOOK_FROM_BEFORE_THE_IMPORT)
    assert done.returncode == 0, done.stderr
    assert done.stdout.strip() == "flushed"


FORK_WHILE_A_RUST_THREAD_WAITS_FOR_PYTHON = """
import asyncio, os, sys, time
from netweir import _native

async def start():
    return _native._test_sleep(0.01)

loop = asyncio.new_event_loop()
loop.run_until_complete(start())
# Keep the GIL for a while: the Rust thread handing the result over is
# waiting for it, counted as inside Python, when the fork happens.
sys.setswitchinterval(30)
end = time.monotonic() + 0.3
while time.monotonic() < end:
    pass
pid = os.fork()
if pid == 0:
    sys.exit(0)  # the child exits through atexit; that thread isn't in it
sys.setswitchinterval(0.005)
start = time.monotonic()
while time.monotonic() - start < 5:
    if os.waitpid(pid, os.WNOHANG)[0]:
        print(f"{time.monotonic() - start:.2f}")
        break
    time.sleep(0.01)
else:
    os.kill(pid, 9)
    print("hung")
"""


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
def test_a_forked_child_exits_without_waiting_for_its_parents_threads():
    done = run_python(FORK_WHILE_A_RUST_THREAD_WAITS_FOR_PYTHON)
    assert done.returncode == 0, done.stderr
    assert done.stdout.strip() != "hung"
    assert float(done.stdout) < 0.5
