#!/usr/bin/env python3
"""Exact explicit-module JSON tree differential against official Bazel 9.3.0.

Stdlib-only; private workspaces/output roots; real built-ins are never replaced.
The reference may access BCR and execute genuine built-in extensions. Native
does neither. PASS is scoped to the listed cases, not full Bzlmod parity.
"""

import argparse
import datetime
import importlib.util
import json
from pathlib import Path
import shutil
import signal
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]
_SPEC = importlib.util.spec_from_file_location("module_validation_support", ROOT / "scripts/validate.py")
support = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(support)
FIXTURES = ROOT / "tests/fixtures/modules/cases.json"
PROFILE = ["--output=json", "--noinclude_builtin", "--extension_info=hidden", "--lockfile_mode=off"]


def load_fixtures(path=FIXTURES):
    data = json.loads(path.read_text())
    if not data["namespace"].startswith("opencode_modcheck_"):
        raise ValueError("fixture names must use the private module namespace")
    names = [case["name"] for case in data["cases"]]
    if not names or len(names) != len(set(names)):
        raise ValueError("fixture case names must be nonempty and unique")
    return data


def module_text(name, version, body=""):
    return 'module(name = {}, version = {})\n'.format(json.dumps(name), json.dumps(version)) + body


def materialize(scratch, fixture):
    """Generate fresh absolute local sources; never touch the source fixtures."""
    scratch = scratch.resolve()
    registry, workspace, override = (scratch / name for name in ("registry", "workspace", "override"))
    for path in (registry, workspace, override):
        path.mkdir(parents=True)
    (registry / "bazel_registry.json").write_text("{}\n")
    namespace = fixture["namespace"]
    versions = {}
    for key, body in fixture["modules"].items():
        short_name, version = key.split("@")
        if any(part in ("", ".", "..") or "/" in part or "\\" in part
               for part in (short_name, version)):
            raise ValueError("unsafe fixture module key: " + key)
        name = namespace + short_name
        entry = registry / "modules" / name / version
        source = scratch / "sources" / name / version
        entry.mkdir(parents=True)
        source.mkdir(parents=True)
        text = module_text(name, version, body.format(ns=namespace))
        (entry / "MODULE.bazel").write_text(text)
        (source / "MODULE.bazel").write_text(text)
        (source / "BUILD.bazel").write_text("")
        (entry / "source.json").write_text(json.dumps({"type": "local_path", "path": str(source)}) + "\n")
        versions.setdefault(name, []).append(version)
    for name, available in versions.items():
        (registry / "modules" / name / "metadata.json").write_text(json.dumps({
            "versions": sorted(available), "yanked_versions": {},
        }) + "\n")
    (override / "MODULE.bazel").write_text(fixture["override"].format(ns=namespace))
    (override / "BUILD.bazel").write_text("")
    (workspace / "BUILD.bazel").write_text("")
    return workspace, registry, override


def root_text(case, fixture, override):
    return module_text(fixture["namespace"] + "root", "1.0", case["root"].format(
        ns=fixture["namespace"], override_path=json.dumps(str(override.resolve()))))


def expected_node(spec, namespace):
    """Expand a hand-authored expectation, not a second resolution algorithm."""
    name, version = spec["module"].split("@")
    name = namespace + name
    node = {"key": name + "@" + version, "name": name,
            "version": spec.get("version", version), "apparentName": spec.get("alias", name)}
    if spec.get("unexpanded"):
        node["unexpanded"] = True
    else:
        for field in ("dependencies", "indirectDependencies", "cycles"):
            node[field] = [expected_node(child, namespace) for child in spec.get(field, [])]
    return node


def expected_graph(case, fixture):
    name = fixture["namespace"] + "root"
    return {"key": "<root>", "name": name, "version": "1.0", "apparentName": name,
            "root": True, "dependencies": [expected_node(spec, fixture["namespace"])
                                            for spec in case["expected"]],
            "indirectDependencies": [], "cycles": []}


def parse_graph(record):
    value = json.loads(support.require_success(record))
    if not isinstance(value, dict) or value.get("root") is not True or value.get("key") != "<root>":
        raise ValueError("mod graph stdout is not a rooted JSON graph")
    return value


def compare_graphs(native, reference, expected):
    """No projection: extra built-in root deps or changed array order fail."""
    if native != reference:
        raise ValueError("native/reference parsed JSON trees differ (no fields or dependencies filtered)")
    if reference != expected:
        raise ValueError("both JSON trees differ from the hand-authored fixture expectation")


def require_rejection(record, diagnostic):
    code = record["returncode"]
    if (code is None or code <= 0 or code >= 128 or code == 101 or record["timed_out"]
            or record.get("spawn_error") or record.get("interrupted")
            or record["stdout"].strip() or "panicked" in record["stderr"].lower()
            or diagnostic.lower() not in record["stderr"].lower()):
        raise ValueError("expected controlled native rejection containing " + repr(diagnostic))


def native_command(executable, registry, flags=()):
    # The explicit-module profile: the fixture registry has no built-in closure
    # (genuine's --noinclude_builtin hides it).
    return [str(executable), "--ignore_all_rc_files", "mod", "graph"] + PROFILE + [
        "--experimental-builtin-modules=false", "--registry=" + registry.as_uri()] + list(flags)


def reference_graph(reference, name, registry, flags=()):
    reference.started = True
    # Reference.run puts flags before the mod subcommand. Keep graph first here.
    return reference.runner.run(name, reference.startup + ["mod", "graph"] + list(
        support.COMMON_FLAGS) + PROFILE + ["--registry=" + registry.as_uri(),
                                         "--registry=https://bcr.bazel.build"] + list(flags),
                                reference.workspace)


class ReferenceFailure(ValueError):
    """A failing reference is not evidence of native parity or non-parity."""


def differential(case, fixture, override, registry, native, reference, runner):
    (reference.workspace / "MODULE.bazel").write_text(root_text(case, fixture, override))
    flags = case.get("flags", [])
    item = {"name": case["name"], "kind": "differential", "status": "FAIL",
            "expected": expected_graph(case, fixture)}
    runner.report["checks"].append(item)
    native_run = runner.run(case["name"] + ".native", native_command(native, registry, flags), reference.workspace)
    item["native_run_id"] = native_run["id"]
    reference_run = reference_graph(reference, case["name"] + ".reference", registry, flags)
    item["reference_run_id"] = reference_run["id"]
    try:
        item["reference"] = parse_graph(reference_run)
    except (ValueError, OSError) as exc:
        item["status"] = "REFERENCE_FAILURE"
        item["error"] = str(exc)
        raise ReferenceFailure("official reference failed for " + case["name"] + "; inspect recorded stderr") from exc
    try:
        item["native"] = parse_graph(native_run)
        compare_graphs(item["native"], item["reference"], item["expected"])
        item["status"] = "PASS"
    except (ValueError, OSError) as exc:
        item["error"] = str(exc)


def boundary_probes(fixture, registry, native, workspace, runner):
    """Capability checks only; never equate native rejection with official failure."""
    root = module_text(fixture["namespace"] + "root", "1.0")
    probes = [
        ("missing_module", root + 'bazel_dep(name = "' + fixture["namespace"] + 'absent", version = "1.0")\n', [], "not found"),
        ("unknown_option", root, ["--invented_module_flag"], "unknown"),
        ("unsupported_registry_scheme", root, ["--registry=ftp://unsupported.invalid/registry"], "unsupported URL"),
        ("unsupported_output", root, ["--output=text"], "unsupported"),
        ("extension_pending", root + 'ext = use_extension("//:extension.bzl", "ext")\n', [], "cannot execute module extensions"),
    ]
    # A real extension exists; the native rejection is capability-only, not an
    # invalid-label trick. Do not run it on the reference or call this parity.
    (workspace / "extension.bzl").write_text("def _impl(ctx):\n    pass\next = module_extension(implementation = _impl)\n")
    for name, text, flags, diagnostic in probes:
        (workspace / "MODULE.bazel").write_text(text)
        record = runner.run("boundary." + name, native_command(native, registry, flags), workspace)
        item = {"name": name, "kind": "native_capability_boundary", "run_id": record["id"], "status": "FAIL"}
        runner.report["checks"].append(item)
        try:
            require_rejection(record, diagnostic)
            item["status"] = "PASS"
            item["details"] = "Controlled native rejection only; not reference parity."
        except ValueError as exc:
            item["error"] = str(exc)


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=ROOT / "target/release/bazel-rs")
    parser.add_argument("--reference", type=Path, default=ROOT / ".direnv/reference/bazel-9.3.0-linux-x86_64")
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/module-validation.json")
    parser.add_argument("--timeout", type=support.positive_seconds, default=180)
    return parser


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ("native", "reference", "output"):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {"schema_version": 1, "status": "FAIL", "runs": [], "checks": [], "errors": [], "cleanup": [],
              "scratch_cleaned": False, "reference_version_required": "9.3.0",
              "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "configuration": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              "profile": PROFILE, "comparison": "exact parsed JSON, including array order; no graph projection",
              "limitations": ["Explicit-root selected JSON tree only; not full Bzlmod parity.",
                              "No fake bazel_tools; genuine built-in closure uses BCR and may execute extensions.",
                              "Native does not inject built-ins, fetch sources, or execute extensions.",
                              "Canonical names and nodep mappings require resolver unit tests; not visible in JSON.",
                              "Native capability rejections are not negative differential parity.",
                              "Private output roots; remote cache, executor and BES disabled; not a network sandbox."],
              "test_commands_executed": False}
    runner = support.Runner(report, args.timeout)
    scratch = reference = None
    interrupted = False
    expected_checks = 0
    try:
        support.validate_executable(args.native)
        support.validate_executable(args.reference, official=True)
        report["executables"] = {"native": support.file_evidence(args.native), "reference": support.file_evidence(args.reference)}
        fixture = load_fixtures()
        report["fixture"] = support.file_evidence(FIXTURES)
        report["planned_differential_cases"] = [case["name"] for case in fixture["cases"]]
        expected_checks = len(fixture["cases"]) + 5
        scratch = Path(tempfile.mkdtemp(prefix="bazel-modules-", dir="/tmp/opencode"))
        report["scratch_directory"] = str(scratch)
        workspace, registry, override = materialize(scratch, fixture)
        (workspace / "MODULE.bazel").write_text(root_text(fixture["cases"][0], fixture, override))
        reference = support.Reference(args.reference, workspace, scratch, runner, "modules")
        reference.started = True
        version = runner.run("reference.version", reference.startup + ["version", "--gnu_format"], workspace)
        report["reference_release"] = support.require_success(version).strip()
        if report["reference_release"] != "bazel 9.3.0":
            raise ValueError("reference is not Bazel 9.3.0")
        for case in fixture["cases"]:
            differential(case, fixture, override, registry, args.native, reference, runner)
        boundary_probes(fixture, registry, args.native, workspace, runner)
    except KeyboardInterrupt:
        interrupted = True
        report["errors"].append("interrupted")
    except (OSError, ValueError) as exc:
        report["errors"].append(str(exc))
    finally:
        if reference is not None:
            try:
                reference.shutdown()
                report["reference_shutdown_attempted"] = reference.started
            except (OSError, ValueError) as exc:
                report["errors"].append("shutdown: " + str(exc))
        if scratch is not None:
            try:
                report["scratch_bytes_before_cleanup"] = support.directory_size(scratch)
            except OSError as exc:
                report["scratch_size_error"] = str(exc)
            try:
                shutil.rmtree(scratch)
                report["scratch_cleaned"] = not scratch.exists()
            except OSError as exc:
                report["errors"].append("scratch cleanup: " + str(exc))
        report["completed_differential_cases"] = sum(item["kind"] == "differential" and item["status"] == "PASS"
                                                      for item in report["checks"])
        if (expected_checks and len(report["checks"]) == expected_checks and not report["errors"]
                and report["scratch_cleaned"] and all(item["status"] == "PASS" for item in report["checks"])):
            report["status"] = "PASS"
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        support.write_report(args.output, report)
    print("{} (explicit-module profile only): {}".format(report["status"], args.output))
    return 130 if interrupted else (0 if report["status"] == "PASS" else 1)


if __name__ == "__main__":
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
