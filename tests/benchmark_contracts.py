#!/usr/bin/env python3
"""Regression checks for the matched benchmark's syscall audit."""
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from benchmark_matched import completed_syscalls


class SyscallRecords(unittest.TestCase):
    def test_interleaved_success_and_failed_path_lookup(self):
        lines = [
            '100 execve("/missing/true", ["true"], 0x1) = -1 ENOENT (No such file or directory)',
            '100 execve("/usr/bin/true", ["true"], 0x1 <unfinished ...>',
            '101 execve("/usr/bin/true", ["true"], 0x2 <unfinished ...>',
            '101 <... execve resumed>) = 0',
            '100 <... execve resumed>) = 0',
        ]
        records = list(completed_syscalls(lines))
        self.assertEqual(sum(r.startswith("execve(") and r.endswith("= 0") for r in records), 2)

    def test_split_nonblocking_connection_keeps_destination(self):
        lines = [
            '100 connect(3, {sa_family=AF_UNIX, sun_path="/fixture/host.sock"}, 24 <unfinished ...>',
            '101 execve("/usr/bin/true", ["true"], 0x2) = 0',
            '100 <... connect resumed>) = -1 EINPROGRESS (Operation now in progress)',
        ]
        records = list(completed_syscalls(lines))
        self.assertEqual(sum(r.startswith("connect(") and "/fixture/host.sock" in r for r in records), 1)

    def test_incomplete_trace_is_not_silently_counted(self):
        with self.assertRaises(AssertionError):
            list(completed_syscalls(['100 <... execve resumed>) = 0']))


if __name__ == "__main__":
    unittest.main(verbosity=2)
