"""How much memory this process has used at most: its peak resident set,
which is what a memory limit is about (Scrapy's MEMUSAGE reads the same)."""

from __future__ import annotations

import sys


def peak() -> int | None:
    """The process's peak resident memory in bytes, or None where it can't
    be read."""
    if sys.platform == "win32":
        return _windows_peak()
    if sys.platform.startswith("linux"):
        # Not ru_maxrss: Linux keeps that across exec, so a worker process
        # (started by fork and exec) would report its parent's peak as its
        # own. VmHWM is this process's alone.
        try:
            with open("/proc/self/status", encoding="ascii", errors="replace") as f:
                for line in f:
                    if line.startswith("VmHWM:"):
                        return int(line.split()[1]) * 1024
        except (OSError, ValueError, IndexError):
            pass
    try:
        import resource
    except ImportError:
        return None
    used = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    # Linux and the BSDs count kilobytes; macOS counts bytes.
    return used if sys.platform == "darwin" else used * 1024


def _windows_peak() -> int | None:
    import ctypes
    from ctypes import wintypes

    class Counters(ctypes.Structure):
        _fields_ = [
            ("cb", wintypes.DWORD),
            ("PageFaultCount", wintypes.DWORD),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
        ]

    counters = Counters()
    counters.cb = ctypes.sizeof(Counters)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetCurrentProcess.restype = wintypes.HANDLE
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD]
    psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
    if not psapi.GetProcessMemoryInfo(
        kernel32.GetCurrentProcess(), ctypes.byref(counters), counters.cb
    ):
        return None
    return counters.PeakWorkingSetSize
