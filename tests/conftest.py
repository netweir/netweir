import pytest

from netweir import _cli


@pytest.fixture(autouse=True)
def _one_folder_of_spiders_per_test():
    """A process loads spider files from one folder; each test is a
    process of its own as far as that goes."""
    yield
    _cli._forget_loaded()
