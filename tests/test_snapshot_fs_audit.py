"""Offline tests of scripts/snapshot_fs_audit.py's trace parsing and coverage rules."""

import os
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
        paths = [p for _, _, ps, _, _ in events for _, p in ps]
        self.assertIn("/ws/BUILD.bazel", paths)
        self.assertIn("/ws/pkg/x y", paths)
        pairs = [x for _, _, ps, _, _ in events for x in ps]
        self.assertIn(("path", "/ws/pkg"), pairs)
        self.assertIn(("dir", "/ws/pkg"), pairs)

    def test_only_unrecorded_paths_inside_the_window_are_reported(self):
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(DUMP)
            touched, unrecorded = audit.audit(TRACE, dump, "/ws", "/cache", [])
        self.assertEqual([u.split()[0] for u in unrecorded], ["/ws/unrecorded.txt"])
        self.assertGreater(touched, 5)

    def test_tainted_sections_and_started_processes_are_skipped(self):
        trace = TRACE.replace(
            '101 statx(AT_FDCWD</ws>, "/ws/unrecorded.txt"',
            '101 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/tainted-begin", 0x1, 0) = -1 ENOENT\n'
            '102 newfstatat(AT_FDCWD</ws>, "/ws/other-thread.txt", 0x1, 0) = 0\n'
            '101 clone3({flags=CLONE_VM|CLONE_VFORK, exit_signal=SIGCHLD, stack=0x1, stack_size=0x9000}, 88 <unfinished ...>\n'
            '103 execve("/usr/bin/java", ["java"], 0x1 /* 3 vars */) = 0\n'
            '101 <... clone3 resumed>) = 103\n'
            '103 clone3({flags=CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM, stack=0x1}, 88) = 104\n'
            '104 openat(AT_FDCWD</ws>, "/usr/lib/jvm/lib/libjvm.so", O_RDONLY) = 3</usr/lib/jvm/lib/libjvm.so>\n'
            '101 statx(AT_FDCWD</ws>, "/usr/lib/jvm/release", 0, 0x1) = 0\n'
            '101 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/tainted-end", 0x1, 0) = -1 ENOENT\n'
            '101 statx(AT_FDCWD</ws>, "/ws/unrecorded.txt"')
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(DUMP)
            _, unrecorded = audit.audit(trace, dump, "/ws", "/cache", [])
        self.assertEqual([u.split()[0] for u in unrecorded], ["/ws/other-thread.txt", "/ws/unrecorded.txt"])

    def test_listings_through_links_and_realpath_chains_are_covered(self):
        with tempfile.TemporaryDirectory() as temp:
            host = Path(temp) / "host"
            (host / "jdk" / "conf").mkdir(parents=True)
            (host / "etc").mkdir()
            (host / "etc" / "net.properties").write_text("x")
            os.symlink(host / "etc" / "net.properties", host / "jdk" / "conf" / "net.properties")
            tree = Path(temp) / "tree"
            tree.mkdir()
            os.symlink(host / "jdk" / "conf", tree / "conf")
            trace = (
                f'1 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/window-open", 0x1, 0) = -1 ENOENT\n'
                f'1 openat(AT_FDCWD</ws>, "{tree}/conf", O_RDONLY|O_DIRECTORY|O_CLOEXEC) = 7<{host}/jdk/conf>\n'
                f'1 getdents64(7<{host}/jdk/conf>, 0x1 /* 3 entries */, 32768) = 80\n'
                f'1 readlink("{tree}/conf", "{host}/jdk/conf", 1023) = 9\n'
                f'1 readlink("{host}/jdk/conf/net.properties", "{host}/etc/net.properties", 1023) = 9\n'
                f'1 readlink("{host}/etc/net.properties", 0x1, 1023) = -1 EINVAL (Invalid argument)\n'
                f'1 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/window-close", 0x1, 0) = -1 ENOENT\n'
            )
            dump = Path(temp) / "dump.txt"
            dump.write_text(
                f"OBS {tree}/conf READDIR[all] entries:1 digest:00 key:1:2:3:4:5\n"
                f"OBS {tree}/conf/net.properties CANON path:{host}/etc/net.properties\n"
            )
            touched, unrecorded = audit.audit(trace, dump, "/ws", "/cache", [])
        self.assertEqual(unrecorded, [])
        self.assertEqual(touched, 5)

    def test_replay_touches_recorded_paths_stages_pinned_roots_and_impure_host_paths(self):
        replay_trace = TRACE + (
            '100 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/replay-begin", 0x1, 0) = -1 ENOENT\n'
            # A definition load: a recorded .bzl, the cache, a pinned root.
            '105 openat(AT_FDCWD</ws>, "/ws/BUILD.bazel", O_RDONLY|O_CLOEXEC) = 3</ws/BUILD.bazel>\n'
            '105 statx(AT_FDCWD</ws>, "/cache/repositories/direct/stages/x/BUILD", 0, 0x1) = 0\n'
            '105 statx(AT_FDCWD</ws>, "/pinned/local/defs.bzl", 0, 0x1) = 0\n'
            # A pure rule's execution must not read the host...
            '105 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/replay-exec/pure+repo", 0x1, 0) = -1 ENOENT\n'
            '105 statx(AT_FDCWD</ws>, "/usr/share/pure-read", 0, 0x1) = 0\n'
            '105 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/replay-exec-end", 0x1, 0) = -1 ENOENT\n'
            # ... an impure one may (which, execute, environ).
            '106 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/replay-exec/impure+jdk", 0x1, 0) = -1 ENOENT\n'
            '106 statx(AT_FDCWD</ws>, "/usr/lib/jvm/release", 0, 0x1) = 0\n'
            '106 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/replay-exec-end", 0x1, 0) = -1 ENOENT\n'
            '105 statx(AT_FDCWD</ws>, "/ws/unrecorded-in-replay", 0, 0x1) = 0\n'
            '100 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/replay-end", 0x1, 0) = -1 ENOENT\n'
        )
        dump_text = DUMP + (
            "ROOT local+repo /pinned/local generated:1\n"
            "REPLAY pure+repo match definition:x flags:- output:0 recorded:0.1 replay:0.1 cpu:- root:/c/t\n"
            "REPLAY impure+jdk match definition:y flags:environ,execute,which output:0 recorded:50 replay:60 cpu:- root:/c/u\n"
        )
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(dump_text)
            replay = {}
            _, unrecorded = audit.audit(replay_trace, dump, "/ws", "/cache", [], replay)
        self.assertEqual([u.split()[0] for u in unrecorded], ["/ws/unrecorded.txt"])
        self.assertTrue(replay["present"])
        self.assertEqual(replay["executions"], 2)
        self.assertEqual([u.split()[0] for u in replay["unrecorded"]],
                         ["/usr/share/pure-read", "/ws/unrecorded-in-replay"])

    def test_missing_markers_fail(self):
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(DUMP)
            touched, problems = audit.audit("1 statx(AT_FDCWD, \"/x\", 0) = 0\n", dump, "/ws", "/cache", [])
        self.assertIsNone(touched)
        self.assertTrue(problems)


if __name__ == "__main__":
    unittest.main()
