"""Independent OS probes for Linux/macOS runtime assertions (test-only)."""
import ctypes
import os
from pathlib import Path
import subprocess
import sys


def procargs(pid):
    library = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    argmax = ctypes.c_int()
    size = ctypes.c_size_t(ctypes.sizeof(argmax))
    if library.sysctlbyname(b"kern.argmax", ctypes.byref(argmax), ctypes.byref(size), None, 0):
        raise OSError(ctypes.get_errno(), "kern.argmax")
    buffer = ctypes.create_string_buffer(argmax.value)
    size = ctypes.c_size_t(len(buffer))
    mib = (ctypes.c_int * 3)(1, 49, pid)
    if library.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0):
        raise OSError(ctypes.get_errno(), "KERN_PROCARGS2")
    raw = buffer.raw[:size.value]
    argc = int.from_bytes(raw[:4], sys.byteorder)
    offset = raw.index(b"\0", 4) + 1
    while raw[offset] == 0:
        offset += 1
    parts = raw[offset:].split(b"\0")
    return parts[:argc], [part for part in parts[argc:] if part]


def argv(pid):
    if sys.platform == "darwin":
        return procargs(pid)[0]
    return Path(f"/proc/{pid}/cmdline").read_bytes().rstrip(b"\0").split(b"\0")


def environment(pid):
    if sys.platform == "darwin":
        return procargs(pid)[1]
    return Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")


def executable_name(pid):
    if pid is None:
        raise FileNotFoundError("foreground process not ready")
    if sys.platform == "darwin":
        output = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "comm="], text=True)
        return Path(output.strip()).name
    return Path(os.readlink(f"/proc/{pid}/exe")).name


def fd_target(pid, fd):
    if sys.platform == "darwin":
        output = subprocess.check_output(
            ["/usr/sbin/lsof", "-a", "-p", str(pid), "-d", str(fd), "-Ftn"], text=True)
        if "tPIPE" in output.splitlines():
            return "pipe:"
        return next(line[1:] for line in output.splitlines() if line.startswith("n"))
    return os.readlink(f"/proc/{pid}/fd/{fd}")
