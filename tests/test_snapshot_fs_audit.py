"""Offline tests of scripts/snapshot_fs_audit.py's trace parsing and coverage rules."""

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import snapshot_fs_audit as audit


TRACE = """\
100 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/window-open", 0x7ff, AT_SYMLINK_NOFOLLOW) = -1 ENOENT (No such file or directory)
100 openat(AT_FDCWD</ws>, "BUILD.bazel", O_RDONLY|O_CLOEXEC) = 3</ws/BUILD.bazel>
101 statx(AT_FDCWD</ws>, "/ws/pkg", AT_STATX_SYNC_AS_STAT, STATX_ALL, {stx_mode=S_IFDIR|0755, ...}) = 0
101 openat(AT_FDCWD</ws>, "/ws/pkg", O_RDONLY|O_NONBLOCK|O_CLOEXEC|O_DIRECTORY) = 4</ws/pkg>
101 getdents64(4</ws/pkg>, 0x55 /* 3 entries */, 32768) = 80
101 readlink("/ws", 0x7ffe, 1023) = -1 EINVAL (Invalid argument)
101 openat(5</ws/pkg>, "x\\040y", O_RDONLY) = -1 ENOENT (No such file or directory)
101 statx(AT_FDCWD</ws>, "/cache/repositories/trees/k/BUILD", AT_STATX_SYNC_AS_STAT, STATX_ALL, 0x1) = 0
101 statx(AT_FDCWD</ws>, "/ext/root/sub/file", AT_STATX_SYNC_AS_STAT, STATX_ALL, 0x1) = 0
101 openat(AT_FDCWD</ws>, "/proc/self/status", O_RDONLY) = 5</proc/1/status>
101 statx(AT_FDCWD</ws>, "/ws/unrecorded.txt", AT_STATX_SYNC_AS_STAT, STATX_ALL, 0x1) = 0
100 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/window-close", 0x7ff, AT_SYMLINK_NOFOLLOW) = -1 ENOENT (No such file or directory)
100 statx(AT_FDCWD</ws>, "/ws/after-window", AT_STATX_SYNC_AS_STAT, STATX_ALL, 0x1) = 0
"""

DUMP = """\
# bazel-rs snapshot recording (L2-W3) v1
OBS /ws/BUILD.bazel READ sha256:00 key:1:2:3:4:5
OBS /ws/pkg READDIR[glob:**/*.go@] entries:1 digest:00 key:1:2:3:4:5
OBS /ws/pkg STAT kind:d
OBS /ws/pkg/x\\x20y LSTAT error:2
OBS /ws/BUILD.bazel CANON path:/ws/BUILD.bazel
REPO registry r@1 /ext/root manifest:/ext/root.json entries:2
INT /ext/root d key:1:2:3:4:5
"""


class SnapshotAuditTests(unittest.TestCase):
    def test_dump_tokens_unescape_the_rust_escape(self):
        self.assertEqual(audit.unescape_dump("a\\x20b\\x5cc"), "a b\\c")

    def test_trace_paths_resolve_cwd_and_dirfd_relative_names(self):
        events = audit.parse_strace(TRACE, "/ws")
        paths = [p for _, _, ps, _ in events for _, p in ps]
        self.assertIn("/ws/BUILD.bazel", paths)
        self.assertIn("/ws/pkg/x y", paths)
        pairs = [x for _, _, ps, _ in events for x in ps]
        self.assertIn(("path", "/ws/pkg"), pairs)
        self.assertIn(("dir", "/ws/pkg"), pairs)

    def test_only_unrecorded_paths_inside_the_window_are_reported(self):
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(DUMP)
            touched, unrecorded = audit.audit(TRACE, dump, "/ws", "/cache", [])
        self.assertEqual([u.split()[0] for u in unrecorded], ["/ws/unrecorded.txt"])
        self.assertGreater(touched, 5)

    def test_missing_markers_fail(self):
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(DUMP)
            touched, problems = audit.audit("1 statx(AT_FDCWD, \"/x\", 0) = 0\n", dump, "/ws", "/cache", [])
        self.assertIsNone(touched)
        self.assertTrue(problems)


if __name__ == "__main__":
    unittest.main()
