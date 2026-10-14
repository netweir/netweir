"""`netweir install chrome`, against a local stand-in for Google's Chrome
for Testing index."""

import io
import json
import platform
import sys
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir
from netweir._cli import main
from netweir._native import install_chrome


def cft_platform():
    machine = platform.machine().lower()
    arm = machine in ("arm64", "aarch64")
    if sys.platform == "darwin":
        return "mac-arm64" if arm else "mac-x64"
    if sys.platform.startswith("linux"):
        return "linux-arm64" if arm else "linux64"
    return "win64"


def executable(plat):
    if plat.startswith("mac"):
        return (
            f"chrome-{plat}/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
        )
    return f"chrome-{plat}/chrome" + (".exe" if plat.startswith("win") else "")


@pytest.fixture
def index():
    plat = cft_platform()
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w") as z:
        z.writestr(executable(plat), "#!/bin/sh\n")
    files = {"/chrome.zip": archive.getvalue()}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            body = files.get(self.path)
            self.send_response(200 if body else 404)
            body = body or b"missing"
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    base = f"http://127.0.0.1:{server.server_port}"
    # Every milestone, so the test doesn't depend on which one netweir's
    # newest profile is.
    files["/index.json"] = json.dumps(
        {
            "milestones": {
                str(m): {
                    "version": f"{m}.0.1.2",
                    "downloads": {"chrome": [{"platform": plat, "url": base + "/chrome.zip"}]},
                }
                for m in range(100, 300)
            }
        }
    ).encode()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield base + "/index.json"
    server.shutdown()
    server.server_close()


def test_chrome_installs_into_netweirs_folder(index, tmp_path):
    version, path, already = install_chrome(index=index, home=tmp_path)
    assert version.endswith(".0.1.2") and not already
    assert str(path).startswith(str(tmp_path / "chrome" / version))
    assert install_chrome(index=index, home=tmp_path)[2] is True


def test_a_failed_install_is_a_browser_error(tmp_path):
    with pytest.raises(netweir.BrowserError):
        install_chrome(index="http://127.0.0.1:9/index.json", home=tmp_path)


def test_the_command_installs_only_chrome():
    with pytest.raises(SystemExit) as stopped:
        main(["install", "firefox"])
    assert stopped.value.code == 2
