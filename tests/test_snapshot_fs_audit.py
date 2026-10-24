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

    def test_files_opened_for_reading_need_a_read_record_and_tree_links_no_exemption(self):
        trace = (
            '1 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/window-open", 0x1, 0) = -1 ENOENT\n'
            # Read with a READ record; read with only a STAT record.
            '1 openat(AT_FDCWD</ws>, "/ws/read.bzl", O_RDONLY|O_CLOEXEC) = 3</ws/read.bzl>\n'
            '1 openat(AT_FDCWD</ws>, "/ws/stat-only.txt", O_RDONLY|O_CLOEXEC) = 3</ws/stat-only.txt>\n'
            # Directory, O_PATH and write opens need coverage only.
            '1 openat(AT_FDCWD</ws>, "/ws/dir", O_RDONLY|O_NONBLOCK|O_CLOEXEC|O_DIRECTORY) = 4</ws/dir>\n'
            '1 openat(AT_FDCWD</ws>, "/ws/handle", O_RDONLY|O_PATH|O_CLOEXEC) = 4</ws/handle>\n'
            '1 openat(AT_FDCWD</ws>, "/ws/written", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 4</ws/written>\n'
            # A failed open is no read.
            '1 openat(AT_FDCWD</ws>, "/ws/missing", O_RDONLY|O_CLOEXEC) = -1 ENOENT (No such file or directory)\n'
            # Inside a verified tree: covered by the tree...
            '1 openat(AT_FDCWD</ws>, "/c/trees/t/BUILD", O_RDONLY|O_CLOEXEC) = 3</c/trees/t/BUILD>\n'
            '1 statx(AT_FDCWD</ws>, "/c/trees/t/sub/x", 0, 0x1) = 0\n'
            # ... but not below one of its links (a host directory).
            '1 statx(AT_FDCWD</ws>, "/c/trees/t/inc/recorded.h", 0, 0x1) = 0\n'
            '1 statx(AT_FDCWD</ws>, "/c/trees/t/inc/unrecorded.h", 0, 0x1) = 0\n'
            '1 openat(AT_FDCWD</ws>, "/c/trees/t/inc/read.h", O_RDONLY|O_CLOEXEC) = 3</host/x/read.h>\n'
            '1 openat(AT_FDCWD</ws>, "/c/trees/t/inc", O_RDONLY|O_DIRECTORY|O_CLOEXEC) = 5</host/x>\n'
            '1 getdents64(5</host/x>, 0x1 /* 3 entries */, 32768) = 80\n'
            '1 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/window-close", 0x1, 0) = -1 ENOENT\n'
        )
        dump_text = (
            "OBS /ws/read.bzl READ sha256:00 key:1:2:3:4:5\n"
            "OBS /ws/stat-only.txt STAT kind:f\n"
            "OBS /ws/dir STAT kind:d\n"
            "OBS /ws/handle STAT kind:f\n"
            "OBS /ws/written STAT error:2\n"
            "OBS /ws/missing READ error:2\n"
            "OBS /c/trees/t/inc/recorded.h STAT kind:f\n"
            "OBS /c/trees/t/inc/read.h STAT kind:f\n"
            "REPO direct t /c/trees/t manifest:/c/manifests/t.json entries:3\n"
            "INT /c/manifests/t.json f key:1:2:3:4:5\n"
            "INT /c/trees/t d key:1:2:3:4:5\n"
            "INT /c/trees/t/inc l key:1:2:3:4:5\n"
        )
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(dump_text)
            stats = {}
            _, unrecorded = audit.audit(trace, dump, "/ws", "/cache", [], stats=stats)
        self.assertEqual(stats["read_opens"], 4)
        found = {u.split()[0]: u.split()[1] for u in unrecorded}
        self.assertEqual(found, {
            "/ws/stat-only.txt": "[UNREAD",
            "/c/trees/t/inc/unrecorded.h": "[statx]",
            "/c/trees/t/inc/read.h": "[UNREAD",
            # The link itself is a tree entry; listing through it is not.
            "/c/trees/t/inc": "[getdents64]",
        })

    def test_missing_markers_fail(self):
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(DUMP)
            touched, problems = audit.audit("1 statx(AT_FDCWD, \"/x\", 0) = 0\n", dump, "/ws", "/cache", [])
        self.assertIsNone(touched)
        self.assertTrue(problems)

    def test_the_hit_path_touches_only_its_inputs_the_store_and_impure_executions(self):
        trace = (
            '1 statx(AT_FDCWD</ws>, "/ws/before-the-check-point", 0, 0x1) = 0\n'
            '1 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/hit-begin", 0x1, 0) = -1 ENOENT\n'
            # The store and the binary's identity.
            '1 openat(AT_FDCWD</ws>, "/proc/self/exe", O_RDONLY|O_CLOEXEC) = 3</bin/bazel-rs>\n'
            '1 openat(AT_FDCWD</ws>, "/cache/snapshots/v1/k.snap", O_RDONLY|O_CLOEXEC) = 3</cache/snapshots/v1/k.snap>\n'
            # Validation: a recorded read, a recorded stat, a verified tree.
            '1 openat(AT_FDCWD</ws>, "/ws/BUILD.bazel", O_RDONLY|O_CLOEXEC) = 3</ws/BUILD.bazel>\n'
            '1 statx(AT_FDCWD</ws>, "/ws/pkg", 0, 0x1) = 0\n'
            '1 statx(AT_FDCWD</ws>, "/ext/root/sub/file", 0, 0x1) = 0\n'
            # A replay's definition load (a pinned root) and an impure execution.
            '2 openat(AT_FDCWD</ws>, "/pinned/local/defs.bzl", O_RDONLY|O_CLOEXEC) = 4</pinned/local/defs.bzl>\n'
            '2 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/replay-exec/impure+jdk", 0x1, 0) = -1 ENOENT\n'
            '2 statx(AT_FDCWD</ws>, "/usr/lib/jvm/release", 0, 0x1) = 0\n'
            '2 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/replay-exec-end", 0x1, 0) = -1 ENOENT\n'
            # The dump itself (ignored), then an unrecorded read.
            '1 openat(AT_FDCWD</ws>, "/dump.txt", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 5</dump.txt>\n'
            '1 openat(AT_FDCWD</ws>, "/ws/stat-only-read", O_RDONLY|O_CLOEXEC) = 3</ws/stat-only-read>\n'
            '1 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/hit-end", 0x1, 0) = -1 ENOENT\n'
            # Execution.
            '1 statx(AT_FDCWD</ws>, "/ws/an-action-input", 0, 0x1) = 0\n'
        )
        dump_text = DUMP + (
            "OBS /ws/stat-only-read STAT kind:f\n"
            "ROOT local+repo /pinned/local generated:1\n"
            "REPLAY impure+jdk not-replayed definition:y flags:environ,execute,which output:0 recorded:50 replay:- root:/c/u\n"
        )
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(dump_text)
            stats = {}
            touched, unrecorded = audit.audit_hit(trace, dump, "/ws", "/cache", ["/dump.txt"], stats)
            self.assertEqual([u.split()[:2] for u in unrecorded], [["/ws/stat-only-read", "[UNREAD"]])
            self.assertEqual(stats["executions"], 1)
            self.assertEqual(touched, 5)
            # No markers: an error.
            touched, problems = audit.audit_hit("1 statx(AT_FDCWD, \"/x\", 0) = 0\n", dump, "/ws", "/cache", [])
        self.assertIsNone(touched)
        self.assertTrue(problems)

    def test_hit_path_cache_touches_need_a_safe_class_or_coverage(self):
        """L2-W5g: inside the cache dir a touch must be of a safe class (the
        snapshot store, the digest table, the store's directories, a
        replay's stage, a local repository's tree, a verified tree) or be
        covered by the INPUTS (the hidden lockfile); anything else is
        reported."""
        stage, local, tree, other = "a" * 32, "b" * 64, "c" * 64, "d" * 64
        trace = (
            '1 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/hit-begin", 0x1, 0) = -1 ENOENT\n'
            '1 mkdir("/cache/snapshots/v1", 0777) = -1 EEXIST (File exists)\n'
            '1 openat(AT_FDCWD</ws>, "/cache/snapshots/exe.id", O_RDONLY|O_CLOEXEC) = 3</cache/snapshots/exe.id>\n'
            '1 readlink("/cache", 0x1, 1023) = -1 EINVAL (Invalid argument)\n'
            '1 readlink("/cache/repositories", 0x1, 1023) = -1 EINVAL (Invalid argument)\n'
            '1 openat(AT_FDCWD</ws>, "/cache/digests/v1.dmemo", O_RDONLY|O_CLOEXEC) = 3</cache/digests/v1.dmemo>\n'
            f'2 openat(AT_FDCWD</ws>, "/cache/repositories/direct/locks/stage-{stage}", O_RDWR|O_CREAT|O_CLOEXEC, 0666) = 4</x>\n'
            f'2 mkdir("/cache/repositories/direct/stages/{stage}", 0777) = 0\n'
            f'2 openat(AT_FDCWD</ws>, "/cache/repositories/direct/stages/{stage}/BUILD.bazel", O_WRONLY|O_CREAT|O_CLOEXEC, 0666) = 5</x>\n'
            f'2 statx(AT_FDCWD</ws>, "/cache/repositories/direct/trees/{local}", 0, 0x1) = 0\n'
            f'3 openat(AT_FDCWD</ws>, "/cache/repositories/direct/manifests/{local}.json", O_RDONLY|O_CLOEXEC) = 6</x>\n'
            f'3 openat(AT_FDCWD</ws>, "/cache/repositories/direct/trees/{local}/BUILD.bazel", O_RDONLY|O_CLOEXEC) = 7</x>\n'
            f'3 statx(AT_FDCWD</ws>, "/cache/repositories/direct/manifests/{other}.json", 0, 0x1) = 0\n'
            '3 openat(AT_FDCWD</ws>, "/cache/repositories/direct", O_RDONLY|O_CLOEXEC|O_DIRECTORY) = 8</x>\n'
            '3 mkdir("/cache/repositories/trees", 0777) = -1 EEXIST (File exists)\n'
            f'1 statx(AT_FDCWD</ws>, "/cache/repositories/direct/trees/{tree}/sub", 0, 0x1) = 0\n'
            '1 openat(AT_FDCWD</ws>, "/cache/hidden-lockfiles/h/MODULE.bazel.lock", O_RDONLY|O_CLOEXEC) = 3</x>\n'
            '1 openat(AT_FDCWD</ws>, "/cache/repositories/direct/specs/x", O_RDONLY|O_CLOEXEC) = 3</x>\n'
            f'1 statx(AT_FDCWD</ws>, "/cache/repositories/direct/trees/{other}/f", 0, 0x1) = 0\n'
            '1 newfstatat(AT_FDCWD</ws>, "/.bazel-rs-snapshot-marker/hit-end", 0x1, 0) = -1 ENOENT\n'
        )
        dump_text = (
            f"REPO direct t /cache/repositories/direct/trees/{tree} "
            f"manifest:/cache/repositories/direct/manifests/{tree}.json entries:2\n"
            f"INT /cache/repositories/direct/trees/{tree}/sub d key:1:2:3:4:5\n"
            "OBS /cache/hidden-lockfiles/h/MODULE.bazel.lock READ sha256:00 key:1:2:3:4:5\n"
            "REPLAY local+repo not-replayed definition:y flags:- output:0 recorded:1 replay:- "
            f"root:/cache/repositories/direct/trees/{local}\n"
        )
        with tempfile.TemporaryDirectory() as temp:
            dump = Path(temp) / "dump.txt"
            dump.write_text(dump_text)
            stats = {}
            touched, unrecorded = audit.audit_hit(trace, dump, "/ws", "/cache", [], stats)
        self.assertEqual(touched, 4)
        # L2-W5h: an open of a store directory is no metadata call.
        self.assertEqual([u.split()[:2] for u in unrecorded], [
            ["CACHE", "/cache/repositories/direct"],
            ["CACHE", f"/cache/repositories/direct/manifests/{other}.json"],
            ["CACHE", "/cache/repositories/direct/specs/x"],
            ["CACHE", f"/cache/repositories/direct/trees/{other}/f"],
        ])
        self.assertIn("[openat in store-directories]", unrecorded[0])
        # L2-W5h: a local repository's tree and its manifest (checked on
        # every hit).
        self.assertEqual(stats["cache"], {
            "snapshot-store": 2, "cache-root": 1, "store-directories": 2, "digest-table": 1,
            "replay-stage": 3, "local-tree": 3, "verified-tree": 1, "covered-input": 1,
        })


if __name__ == "__main__":
    unittest.main()
