import signal

import pytest

from netweir import _cli


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
