import http.server
import signal
import sys

import pytest
from local_server import socket_state

from netweir import _cli, _serve


@pytest.fixture(autouse=True, scope="session")
def _ctrl_c_reaches_children():
    """Tests that stop a process with SIGINT stand for Ctrl-C at a terminal.
    Started in the background (`pytest &`, nohup), pytest has SIGINT
    ignored, and so would every process it starts: Python keeps it ignored,
    as it should, and those tests would wait out their timeouts. A handled
    signal is reset in a child, so the children get Ctrl-C back."""
    if signal.getsignal(signal.SIGINT) is not signal.SIG_IGN:
        yield
        return
    signal.signal(signal.SIGINT, signal.default_int_handler)
    try:
        yield
    finally:
        signal.signal(signal.SIGINT, signal.SIG_IGN)


@pytest.fixture(autouse=True)
def _one_folder_of_spiders_per_test():
    """A process loads spider files from one folder; each test is a
    process of its own as far as that goes."""
    yield
    _cli._forget_loaded()


@pytest.fixture(autouse=True, scope="session")
def _test_servers_look_up_no_names():
    """http.server looks up a name for the address it binds, which no test
    reads. On a Mac whose resolver doesn't answer for the machine's name,
    one lookup for 127.0.0.1 now and then took 35 s, and CI's macOS runs
    spent minutes on them. Scripts the tests start as processes of their
    own skip it the same way."""
    with pytest.MonkeyPatch.context() as patch:
        patch.setattr(http.server.HTTPServer, "server_bind", _serve._Server.server_bind)
        yield


@pytest.hookimpl(wrapper=True)
def pytest_runtest_makereport(item, call):
    # Windows runners now and then refuse a connection to the test server
    # with WSAENOBUFS. What ran out is only visible while it lasts, so a
    # test failing that way reports the machine's state with its failure.
    report = yield
    if sys.platform == "win32" and report.failed and "ERR_NO_BUFFER_SPACE" in report.longreprtext:
        report.sections.append(("socket state", socket_state()))
    return report
