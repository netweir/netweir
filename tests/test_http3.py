"""HTTP/3 from a built wheel, against the fingerprint crate's capture
server. Runs only with NETWEIR_CAPTURE naming its binary (`cargo build -p
netweir-fingerprint --bin capture`), as the release workflow does for
every wheel; an extension built without the http3 feature fails it."""

import asyncio
import json
import os
import socket
import subprocess

import pytest

import netweir

CAPTURE = os.environ.get("NETWEIR_CAPTURE")

pytestmark = pytest.mark.skipif(not CAPTURE, reason="needs NETWEIR_CAPTURE")


@pytest.fixture
def capture(tmp_path):
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    server = subprocess.Popen(
        [CAPTURE, str(port), "--h3"],
        env={**os.environ, "NETWEIR_HOME": str(tmp_path)},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        for line in server.stderr:
            if line.startswith("listening"):
                break
        else:
            pytest.fail("the capture server stopped before it listened")
        yield port
    finally:
        server.kill()
        server.wait()


def protocol(page):
    return json.loads(page.text)["protocol"]


async def test_firefox_moves_to_http3(capture):
    url = f"https://localhost:{capture}/"
    async with netweir.Client(profile="firefox", verify=False) as client:
        first = await client.get(url)
        # The fetcher proves the alternative first; requests wait for it.
        await asyncio.sleep(1)
        second = await client.get(url)
    assert protocol(first) != "h3"
    assert protocol(second) == "h3"


async def test_chrome_moves_to_http3(capture):
    url = f"https://localhost:{capture}/"
    async with netweir.Client(profile="chrome", verify=False) as client:
        first = await client.get(url)
        assert protocol(first) != "h3"
        # Chrome races QUIC against TCP from the next request on; on
        # localhost either may win the first race, and QUIC the ones after.
        for _ in range(20):
            if protocol(await client.get(url)) == "h3":
                break
            await asyncio.sleep(0.1)
        else:
            pytest.fail("Chrome never moved to HTTP/3")


async def test_http3_off_stays_on_tcp(capture):
    url = f"https://localhost:{capture}/"
    async with netweir.Client(profile="firefox", verify=False, http3=False) as client:
        await client.get(url)
        await asyncio.sleep(1)
        assert protocol(await client.get(url)) != "h3"
