"""`netweir install chrome` and `netweir install firefox`, against local
stand-ins for Google's Chrome for Testing index and Mozilla's archive."""

import hashlib
import io
import json
import platform
import subprocess
import sys
import tarfile
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir
from netweir import _native
from netweir._cli import main
from netweir._native import install_chrome, install_firefox


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


def test_the_command_installs_only_chrome_and_firefox():
    with pytest.raises(SystemExit) as stopped:
        main(["install", "webkit"])
    assert stopped.value.code == 2


def firefox_archive(tmp_path):
    """Mozilla's download for this machine, as (platform, file, bytes),
    holding a stand-in executable; None on Windows, whose download is an
    installer."""
    tree = tmp_path / "tree"
    machine = platform.machine().lower()
    if sys.platform == "darwin":
        exe = tree / "Firefox.app/Contents/MacOS/firefox"
    elif sys.platform.startswith("linux"):
        exe = tree / "firefox/firefox"
    else:
        return None
    exe.parent.mkdir(parents=True)
    exe.write_text("#!/bin/sh\n")
    exe.chmod(0o755)
    if sys.platform == "darwin":
        dmg = tmp_path / "ff.dmg"
        subprocess.run(
            ["hdiutil", "create", "-quiet", "-format", "UDZO", "-srcfolder", tree, dmg],
            check=True,
        )
        return "mac", "Firefox%20159.0.1.dmg", dmg.read_bytes()
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w:xz") as tar:
        tar.add(tree / "firefox", arcname="firefox")
    plat = "linux-aarch64" if machine in ("arm64", "aarch64") else "linux-x86_64"
    return plat, "firefox-159.0.1.tar.xz", archive.getvalue()


@pytest.fixture
def archive(tmp_path):
    made = firefox_archive(tmp_path)
    if made is None:
        pytest.skip("the Windows download is an installer, which can't be faked")
    plat, name, body = made
    files = {f"/159.0.1/{plat}/en-US/{name}": body}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            found = files.get(self.path)
            self.send_response(200 if found else 404)
            found = found or b"missing"
            self.send_header("Content-Length", str(len(found)))
            self.end_headers()
            self.wfile.write(found)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}", [(plat, hashlib.sha256(body).hexdigest())]
    server.shutdown()
    server.server_close()


def test_firefox_installs_into_netweirs_folder(archive, tmp_path):
    base, sums = archive
    home = tmp_path / "home"
    version, path, already = install_firefox(base, home, "159.0.1", sums)
    assert (version, already) == ("159.0.1", False)
    assert str(path).startswith(str(home / "firefox" / "159.0.1"))
    assert install_firefox(base, home, "159.0.1", sums)[2] is True


def test_a_firefox_that_doesnt_match_its_sum_is_a_browser_error(archive, tmp_path):
    base, sums = archive
    wrong = [(plat, "0" * 64) for plat, _ in sums]
    with pytest.raises(netweir.BrowserError, match="SHA-256"):
        install_firefox(base, tmp_path, "159.0.1", wrong)
    assert not any((tmp_path / "firefox").glob("*"))


def test_the_command_installs_firefox(monkeypatch, capsys):
    monkeypatch.setattr(_native, "install_firefox", lambda: ("159.0.1", "/ff/firefox", False))
    assert main(["install", "firefox"]) == 0
    assert capsys.readouterr().out == "Firefox 159.0.1 is installed: /ff/firefox\n"


def test_headless_shell_is_for_chrome_only(capsys):
    assert main(["install", "firefox", "--headless-shell"]) == 2
    assert "--headless-shell" in capsys.readouterr().err
