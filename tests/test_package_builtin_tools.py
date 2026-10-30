"""Miniature packaging fixtures only: no Bazel process, network, or real JDK."""

import contextlib
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import stat
import tarfile
import tempfile
import unittest
from unittest import mock
import warnings
import zipfile


SPEC = importlib.util.spec_from_file_location(
    "package_builtin_tools", Path(__file__).resolve().parents[1]
    / "scripts/package_builtin_tools.py")
packaging = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(packaging)

FILES = {"MODULE.bazel": b'module(name = "bazel_tools")\n',
         "jdk/legal/NOTICE": b"retain notices\n",
         "tools/raw": bytes(range(256)) + b"\x00\xff\r\n"}


def fixture(entries=None, *, label=b"9.3.1", prefix=b"native launcher\x00"):
    if entries is None:
        entries = [("embedded_tools/" + path, data) for path, data in FILES.items()]
    buffer = io.BytesIO(prefix)
    buffer.seek(0, io.SEEK_END)
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", UserWarning)
        with zipfile.ZipFile(buffer, "w") as archive:
            archive.writestr("build-label.txt", label)
            archive.writestr("A-server.jar", b"NEVER package this Java Bazel engine")
            for name, data in entries:
                archive.writestr(name, data)
    data = buffer.getvalue()
    expected = dict(packaging.PINNED,
                    distribution_sha256=hashlib.sha256(data).hexdigest(),
                    module_sha256=hashlib.sha256(FILES["MODULE.bazel"]).hexdigest(),
                    file_count=len(FILES), total_size=sum(map(len, FILES.values())))
    return data, expected


def build(data=None, expected=None):
    if data is None:
        data, expected = fixture()
    return packaging._build_bundle(data, expected=expected)


class PackagingTests(unittest.TestCase):
    def temporary(self):
        temporary = tempfile.TemporaryDirectory(dir="/tmp/opencode")
        self.addCleanup(temporary.cleanup)
        return Path(temporary.name)

    def test_archive_raw_bytes_metadata_and_manifest_schema(self):
        bundle = build()
        manifest = json.loads(bundle[packaging.MANIFEST])
        self.assertEqual(set(manifest), {
            "schema_version", "bazel_version", "platform", "source_revision",
            "distribution_url", "distribution_sha256", "archive_sha256",
            "file_count", "total_size", "files"})
        self.assertEqual(manifest["schema_version"], 1)
        self.assertEqual(manifest["file_count"], len(FILES))
        self.assertEqual(manifest["total_size"], sum(map(len, FILES.values())))
        self.assertEqual(manifest["archive_sha256"],
                         hashlib.sha256(bundle[packaging.ARCHIVE]).hexdigest())
        self.assertEqual(bundle[packaging.MODULE], FILES["MODULE.bazel"])
        raw_tar = gzip.decompress(bundle[packaging.ARCHIVE])
        self.assertEqual(bundle[packaging.ARCHIVE][3:8], b"\x00" * 5)
        with tarfile.open(fileobj=io.BytesIO(raw_tar)) as archive:
            members = archive.getmembers()
            self.assertEqual([member.name for member in members], sorted(FILES))
            for member, record in zip(members, manifest["files"]):
                self.assertTrue(member.isfile())
                self.assertEqual(archive.extractfile(member).read(), FILES[member.name])
                self.assertEqual((member.mode, member.uid, member.gid, member.mtime,
                                  member.uname, member.gname, member.pax_headers),
                                 (0o755, 0, 0, 0, "", "", {}))
                self.assertEqual(record, {
                    "path": member.name, "size": len(FILES[member.name]), "mode": 493,
                    "sha256": hashlib.sha256(FILES[member.name]).hexdigest()})
                # A parser can hide extension records; inspect actual header types.
                self.assertEqual(raw_tar[member.offset + 156:member.offset + 157], b"0")

    def test_deterministic_across_input_order_filename_and_metadata(self):
        root = self.temporary()
        first, trust_a = fixture()
        entries = []
        for path, content in reversed(list(FILES.items())):
            info = zipfile.ZipInfo("embedded_tools/" + path, (2024, 2, 3, 4, 5, 6))
            info.external_attr = (stat.S_IFREG | 0o600) << 16
            entries.append((info, content))
        second, trust_b = fixture(entries, prefix=b"different launcher")
        a, b = root / "first.bin", root / "unrelated.zip"
        a.write_bytes(first)
        b.write_bytes(second)
        bundles = [build(a.read_bytes(), trust_a), build(b.read_bytes(), trust_b)]
        self.assertEqual(bundles[0][packaging.ARCHIVE], bundles[1][packaging.ARCHIVE])
        self.assertEqual(bundles[0][packaging.MODULE], bundles[1][packaging.MODULE])
        manifests = [json.loads(bundle[packaging.MANIFEST]) for bundle in bundles]
        for manifest in manifests:
            del manifest["distribution_sha256"]  # Different source ZIPs have different hashes.
        self.assertEqual(*manifests)
        self.assertEqual(build(first, trust_a), bundles[0])

    def test_unsafe_paths_rejected(self):
        for path in ("/absolute", "embedded_tools/../escape", "embedded_tools/./dot",
                     "embedded_tools//empty", "embedded_tools/", "embedded_tools/\\evil",
                     "C:/escape", "embedded_tools/a\x00hidden"):
            with self.subTest(path=path):
                if path == "embedded_tools/":
                    # A legitimate empty root directory is allowed; data is not.
                    data, trust = fixture([(path, b"not a directory")])
                elif "\x00" in path:
                    info = zipfile.ZipInfo("embedded_tools/safe")
                    info.orig_filename = path
                    with self.assertRaisesRegex(ValueError, "unsafe"):
                        packaging._validate_entry(info)
                    continue
                else:
                    data, trust = fixture([(path, b"bad")])
                with self.assertRaisesRegex(ValueError, "unsafe|nonempty"):
                    build(data, trust)

    def test_duplicates_collisions_links_and_special_files_rejected(self):
        for entries in (
                [("embedded_tools/a", b"a"), ("embedded_tools/a", b"b")],
                [("embedded_tools/a", b"a"), ("embedded_tools/a/b", b"b")],
                [("embedded_tools/a", b"a"), ("embedded_tools/a/", b"")]):
            data, trust = fixture(entries)
            with self.assertRaisesRegex(ValueError, "duplicate|collision"):
                build(data, trust)
        for kind in (stat.S_IFLNK, stat.S_IFIFO, stat.S_IFCHR, stat.S_IFSOCK):
            with self.subTest(kind=kind):
                info = zipfile.ZipInfo("embedded_tools/link")
                info.external_attr = (kind | 0o755) << 16
                data, trust = fixture([(info, b"destination")])
                with self.assertRaisesRegex(ValueError, "link or special"):
                    build(data, trust)

    def test_explicit_zip_directories_are_not_archive_members(self):
        entries = [("embedded_tools/" + path, content) for path, content in FILES.items()]
        entries += [("embedded_tools/", b""), ("embedded_tools/jdk/", b""),
                    ("embedded_tools/jdk/legal/", b"")]
        data, trust = fixture(entries)
        self.assertEqual(build(data, trust)[packaging.ARCHIVE], build()[packaging.ARCHIVE])

    def test_mismatched_trust_fails_before_publication(self):
        data, trust = fixture()
        root = self.temporary()
        distribution = root / "distribution"
        distribution.write_bytes(data)
        output = root / "bundle"
        old = {name: b"old " + name.encode() for name in build()}
        packaging._publish(output, old)
        for field, value in (("distribution_sha256", "0" * 64),
                             ("bazel_version", "wrong"), ("file_count", 999),
                             ("total_size", 999), ("module_sha256", "0" * 64)):
            with self.subTest(field=field):
                expected = dict(trust, **{field: value})
                def miniature(blob, **_):
                    return packaging._build_bundle_original(blob, expected=expected)
                with mock.patch.object(packaging, "_build_bundle_original",
                                       packaging._build_bundle, create=True), \
                        mock.patch.object(packaging, "_build_bundle", side_effect=miniature), \
                        contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(packaging.main([
                        "--distribution", str(distribution), "--output-dir", str(output)]), 1)
                self.assertEqual({p.name: p.read_bytes() for p in output.iterdir()}, old)

    def test_production_cli_cannot_override_trust(self):
        root = self.temporary()
        distribution = root / "distribution"
        distribution.write_bytes(fixture()[0])
        output = root / "absent"
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(packaging.main([
                "--distribution", str(distribution), "--output-dir", str(output)]), 1)
            with self.assertRaises(SystemExit) as caught:
                packaging.main(["--expected-sha256", "0" * 64])
        self.assertEqual(caught.exception.code, 2)
        self.assertFalse(output.exists())

    def test_platform_pins_and_per_platform_defaults(self):
        self.assertEqual(sorted(packaging.PLATFORMS), ["linux-arm64", "linux-x86_64"])
        for platform, pinned in packaging.PLATFORMS.items():
            self.assertEqual(pinned["platform"], platform)
            self.assertTrue(pinned["distribution_url"].endswith(f"/bazel-9.3.1-{platform}"))
            manifest = json.loads((packaging.ROOT / "resources/bazel_tools" / platform
                                   / packaging.MANIFEST).read_text())
            for key in ("bazel_version", "platform", "source_revision", "distribution_url",
                        "distribution_sha256", "file_count", "total_size"):
                self.assertEqual(manifest[key], pinned[key], (platform, key))
        self.assertEqual(packaging.PLATFORMS["linux-arm64"]["module_sha256"],
                         packaging.PINNED["module_sha256"])
        with mock.patch.object(Path, "read_bytes", autospec=True,
                               side_effect=OSError("absent")) as read, \
                contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(packaging.main(["--platform", "linux-arm64", "--check"]), 1)
        self.assertEqual(read.call_args_list[0].args[0],
                         packaging.ROOT / ".direnv/reference/bazel-9.3.1-linux-arm64")

    def test_stage_write_failure_preserves_previous_outputs(self):
        root = self.temporary()
        bundle = build()
        old = {name: b"old" for name in bundle}
        packaging._publish(root, old)
        write = Path.write_bytes
        def fail_manifest(path, data):
            if path.name == packaging.MANIFEST:
                raise OSError("injected staging failure")
            return write(path, data)
        with mock.patch.object(Path, "write_bytes", fail_manifest):
            with self.assertRaisesRegex(OSError, "staging failure"):
                packaging._publish(root, bundle)
        self.assertEqual({p.name: p.read_bytes() for p in root.iterdir()}, old)

    def test_replace_failure_rolls_back_and_cleans_staging(self):
        for existing in (False, True):
            with self.subTest(existing=existing):
                root = self.temporary()
                bundle = build()
                old = {name: b"old" for name in bundle} if existing else {}
                packaging._publish(root, old)
                replace = packaging.os.replace
                def fail_manifest(source, destination):
                    if source.name == packaging.MANIFEST:
                        raise OSError("injected replacement failure")
                    return replace(source, destination)
                with mock.patch.object(packaging.os, "replace", fail_manifest):
                    with self.assertRaisesRegex(OSError, "replacement failure"):
                        packaging._publish(root, bundle)
                self.assertEqual({p.name: p.read_bytes() for p in root.iterdir()}, old)

    def test_check_and_verify_detect_every_changed_output_without_writing(self):
        root = self.temporary()
        bundle = build()
        distribution = root / "distribution"
        distribution.write_bytes(fixture()[0])
        output = root / "bundle"
        packaging._publish(output, bundle)
        for name in bundle:
            for alias in ("--check", "--verify"):
                with self.subTest(name=name, alias=alias):
                    (output / name).write_bytes(bundle[name] + b"altered")
                    before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns)
                              for p in output.iterdir()}
                    with mock.patch.object(packaging, "_build_bundle", return_value=bundle), \
                            mock.patch.object(Path, "write_bytes", side_effect=AssertionError), \
                            contextlib.redirect_stderr(io.StringIO()):
                        self.assertEqual(packaging.main([
                            "--distribution", str(distribution), "--output-dir", str(output),
                            alias]), 1)
                    self.assertEqual({p.name: (p.read_bytes(), p.stat().st_mtime_ns)
                                      for p in output.iterdir()}, before)
                    (output / name).write_bytes(bundle[name])
        packaging._check(output, bundle)
        (output / packaging.MODULE).unlink()
        with self.assertRaisesRegex(ValueError, "MODULE.bazel"):
            packaging._check(output, bundle)

    def test_output_symlinks_are_rejected_without_touching_destination(self):
        root = self.temporary()
        output = root / "bundle"
        output.mkdir()
        target = root / "untouched"
        target.write_bytes(b"keep")
        (output / packaging.MODULE).symlink_to(target)
        with self.assertRaisesRegex(ValueError, "not a regular file"):
            packaging._publish(output, build())
        with self.assertRaisesRegex(ValueError, "bundle differs"):
            packaging._check(output, build())
        self.assertEqual(target.read_bytes(), b"keep")
        self.assertEqual(len(list(output.iterdir())), 1)


if __name__ == "__main__":
    unittest.main()
