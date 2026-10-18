#!/usr/bin/env python3
"""Scoped use_repo_rule MODULE metadata CLI differential, NOT execution parity.

Registry source-file queries probe MODULE metadata and declaration laziness.
(The former native-only nonzero facts_version boundary probe was retired in N52:
facts_version is supported and compared against genuine Bazel by
tests/fixtures/lockfile_extensions/reference_receipt.json. The former
native-only canonical-label output boundary was retired with N39/N51: native
query now prints the apparent display-form label, identical to genuine Bazel,
so query identity checks accept the same spellings for both engines.)
No build/test, wrapper, credential environment changes, fake built-ins, or
generated-repo success claim in this suite. Official Bazel may download its
genuine built-in closure.
"""

import argparse
import base64
import datetime
import hashlib
import json
from pathlib import Path
import re
import shutil
import signal
import sys
import tempfile

from validate import (ROOT, Reference, Runner, file_evidence, positive_seconds,
                      require_success, validate_executable, write_report)


REFERENCE_SHA256 = "d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8"
ROOT_MODULE = ('module(name = "m", version = "1.0")\n'
               'bazel_dep(name = "decldep", version = "1.0", repo_name = "dep")\n')
DEPENDENCY_MODULE = 'module(name = "decldep", version = "1.0")\n'
PROXY = 'repo = use_repo_rule("//:missing.bzl", "unknown_rule")\n'
REAL_PROXY = 'repo = use_repo_rule("//:defs.bzl", "repo_rule")\n'
REAL_DEFS = ('def _repo_impl(ctx):\n'
             '    ctx.file("BUILD.bazel", "exports_files([\\\"data\\\"])\\n")\n'
             '    ctx.file("data", "generated fixture\\n")\n'
             'repo_rule = repository_rule(implementation = _repo_impl)\n'
             'def _ext_impl(ctx):\n'
             '    pass\n'
             'ext = module_extension(implementation = _ext_impl)\n')
LOAD_SENTINEL = "declaration fixture was loaded"
DATA = b"direct repository declaration fixture\x00\xff\n"
BUILD = b'exports_files(["data"])\n'
QUERY = "@dep//:data"
# Canonical identity follows the MODULE name, not the root's apparent alias.
# Since N39 display-form labels and the N51 main-repository mapping, native
# query prints the apparent spelling @dep//:data, matching genuine Bazel; both
# engines accept either spelling of the same single target (differential).
CANONICAL = "@@decldep+//:data"


def case(name, body="", *, dependency=False, ignore=False, expected="query",
         diagnostic=None, root=None, sentinel=False, target=QUERY):
    return {"name": name, "kind": ("reference_laziness_probe" if expected == "laziness"
                                   else "differential"),
            "root_module": root if root is not None else ROOT_MODULE + ("" if dependency else body),
            "dependency_module": DEPENDENCY_MODULE + (body if dependency else ""),
            "flags": ["--ignore_dev_dependency=" + str(ignore).lower()],
            "expected": expected, "diagnostic_class": diagnostic, "target": target,
            "defs": REAL_DEFS + ('fail("' + LOAD_SENTINEL + '")\n' if sentinel else "")}


def cases():
    result = [case("baseline")]
    for name, declaration in (
            ("bad-label", 'use_repo_rule("not a label @@@", "rule")'),
            ("empty-label", 'use_repo_rule("", "rule")'),
            ("empty-rule", 'use_repo_rule("//:missing.bzl", "")'),
            ("private-rule", 'use_repo_rule("//:missing.bzl", "_private")'),
            ("multiword-rule", 'use_repo_rule("//:missing.bzl", "two words")')):
        result.append(case("unused." + name, "unused = " + declaration + "\n"))
    ignored = PROXY + 'repo(name = "generated", dev_dependency = True, foo = repo)\n'
    duplicate_ignored = ignored + 'repo(name = "generated", dev_dependency = True, foo = repo)\n'
    ignored_tag = 'ext=use_extension("//:missing.bzl","ext",dev_dependency=True)\next.tag(value=ext)\n'
    for dependency, ignore, role in ((False, True, "root-ignore"), (True, False, "dependency")):
        result += [case("ignored-tag." + role, ignored_tag, dependency=dependency, ignore=ignore),
                   case("ignored." + role, ignored, dependency=dependency, ignore=ignore),
                   case("ignored-duplicates." + role, duplicate_ignored,
                        dependency=dependency, ignore=ignore),
                   case("ignored-existing-alias." + role,
                        PROXY + 'repo(name = "' + ("decldep" if dependency else "dep") + '", dev_dependency = True, foo = repo)\n',
                        dependency=dependency, ignore=ignore),
                   case("invalid-name-ignored." + role,
                        PROXY + 'repo(name = "_x", dev_dependency = True, foo = repo)\n',
                        dependency=dependency, ignore=ignore, expected="rejection", diagnostic="invalid_name")]
    result.append(case("active-unused.load-probe", REAL_PROXY + 'repo(name = "generated")\n',
                       sentinel=True, expected="laziness"))
    for name, body, diagnostic in (
            ("declaration-zero", 'repo = use_repo_rule()\n', "declaration_signature"),
            ("declaration-one", 'repo = use_repo_rule("//:defs.bzl")\n', "declaration_signature"),
            ("declaration-three", 'repo = use_repo_rule("//:defs.bzl", "repo_rule", "extra")\n', "declaration_signature"),
            ("declaration-keywords", 'repo = use_repo_rule(bzl_file = "//:defs.bzl", rule_name = "repo_rule")\n', "declaration_signature"),
            ("declaration-label-type", 'repo = use_repo_rule(1, "repo_rule")\n', "declaration_signature"),
            ("declaration-rule-type", 'repo = use_repo_rule("//:defs.bzl", 1)\n', "declaration_signature"),
            ("call-positional", PROXY + 'repo("generated", name = "generated")\n', "call_signature"),
            ("call-missing-name", PROXY + 'repo()\n', "name_required"),
            ("call-name-type", PROXY + 'repo(name = 1)\n', "name_type"),
            ("call-dev-type", PROXY + 'repo(name = "generated", dev_dependency = "yes")\n', "dev_type"),
            ("call-invalid-name", PROXY + 'repo(name = "_x")\n', "invalid_name")):
        result.append(case("negative." + name, body, expected="rejection", diagnostic=diagnostic))
    result.append(case("negative.dev-type-before-ignore",
                       PROXY + 'repo(name = "generated", dev_dependency = "yes")\n',
                       ignore=True, expected="rejection", diagnostic="dev_type"))
    for name, body, ignore in (
            ("module-own-alias", REAL_PROXY + 'repo(name = "m")\n', False),
            ("bazel-dep-alias", REAL_PROXY + 'repo(name = "dep")\n', False),
            ("use-repo-alias", 'extension = use_extension("//:defs.bzl", "ext")\nuse_repo(extension, "reserved")\n'
             + REAL_PROXY + 'repo(name = "reserved")\n', False),
            ("ignored-bazel-dep-alias", 'bazel_dep(name = "absentdev", version = "1.0", repo_name = "reserved", dev_dependency = True)\n'
             + REAL_PROXY + 'repo(name = "reserved")\n', True),
            ("ignored-use-repo-alias", 'extension = use_extension("//:defs.bzl", "ext", dev_dependency = True)\nuse_repo(extension, "reserved")\n'
             + REAL_PROXY + 'repo(name = "reserved")\n', True),
            ("direct-duplicate", REAL_PROXY + 'repo(name = "generated")\nrepo(name = "generated")\n', False),
            ("two-proxies", REAL_PROXY + 'other = use_repo_rule("//:defs.bzl", "repo_rule")\nrepo(name = "generated")\nother(name = "generated")\n', False)):
        result.append(case("collision." + name, body, ignore=ignore, expected="rejection", diagnostic="collision"))
    # These are static MODULE grammar checks, including unevaluated expressions.
    # Effective dev-ignore ensures valid call syntax never needs a real .bzl load.
    for name, body, diagnostic in (
            ("kwargs-variable", 'kw={"name":"ignored","dev_dependency":True}\nrepo(**kw)\n', "syntax_kwargs"),
        ("star-literal", 'repo(*[])\n', "syntax_star"),
            ("global-rebinding", 'x=1\nx=2\n', "syntax_rebinding"),
            ("global-augmented-rebinding", 'xs=[]\nxs+=[]\n', "syntax_rebinding"),
            ("dead-comprehension-kwargs", 'kw={}\nunused=[repo(**kw) for item in []]\n', "syntax_kwargs"),
            ("nested-dead-print-kwargs", 'kw={}\nunused=[str(print(**kw)) for item in []]\n', "syntax_kwargs")):
        result.append(case("grammar." + name, PROXY + body, ignore=True,
                           expected="rejection", diagnostic=diagnostic))
    result += [case("grammar.literal-computed-key",
                    PROXY + 'key="name"\nrepo(**{key:"ignored","dev_dependency":True})\n', ignore=True),
               case("grammar.index-mutation",
                    PROXY + 'xs=["before"]\nxs[0]="after"\nrepo(name="ignored",dev_dependency=True,foo=xs)\n',
                    ignore=True)]
    mixed = REAL_PROXY + 'repo(name = "generated", dev_dependency = True)\nrepo(name = "generated")\n'
    result += [case("collision.dev-nondev", mixed, expected="rejection", diagnostic="collision"),
               case("ignored.dev-nondev", mixed, ignore=True),
               case("negative.before-module", root=PROXY + ROOT_MODULE,
                    expected="rejection", diagnostic="module_order")]
    return result


CASES = cases()
PLAN = [(item["name"], item["kind"]) for item in CASES]
DIAGNOSTICS = {
    "declaration_signature": r"use_repo_rule[^\n]*(?:argument|parameter|positional|keyword|type|string)|(?:argument|parameter)[^\n]*(?:bzl_file|rule_name)|\b(?:bzl_file|rule_name) must be a string\b",
    "call_signature": r"(?:unexpected|no|does not accept|got|accepts?)[^\n]*positional|positional[^\n]*(?:argument|not allowed)",
    "name_required": r"(?:missing|required|requires|mandatory)[^\n]*\bname\b|\bname\b[^\n]*(?:required|missing|mandatory)",
    "name_type": r"\bname\b[^\n]*(?:string|str|type)|(?:string|str)[^\n]*\bname\b",
    "dev_type": r"\bdev_dependency\b[^\n]*(?:bool|type)|bool[^\n]*\bdev_dependency\b",
    "invalid_name": r"(?:invalid|valid)[^\n]*(?:repo(?:sitory)? name|user.provided repo)|repo(?:sitory)? name[^\n]*(?:invalid|valid|start|letter)",
    "collision": r"(?:repo(?:sitory)? name|repo(?:sitory)?|name)[^\n]*(?:already (?:being )?(?:used|exists|declared|registered)|collision|duplicate)|(?:duplicate|collision)[^\n]*(?:repo|name)|\brepo name ['\"][^'\"\n]+['\"] cannot be defined[^\n]*already defined\b",
    "module_order": r"module\([^\n]*(?:before|first)|module[^\n]*(?:must be|has to be|called)[^\n]*(?:first|before)|(?:first|before)[^\n]*module\(",
    "syntax_kwargs": r"\*\*kwargs\b[^\n]*(?:must be (?:a )?(?:(?:dict|dictionary) literal|literal (?:dict|dictionary))|(?:not|only)[^\n]*allowed)|(?:only|must)[^\n]*(?:dict|dictionary) literal[^\n]*\*\*kwargs\b",
    "syntax_star": r"(?<!\*)\*args\b[^\n]*(?:not allowed|not supported|forbidden)",
    "syntax_rebinding": r"\bglobal binding\b[^\n]*may not be reassigned|\bcannot reassign global\b|\b(?:global |file.global )?(?:variable|name|identifier)\b[^\n]*(?:is |was )?(?:redeclared|already defined)|'[^'\n]+' redeclared at top level",
    "declaration_loaded": re.escape(LOAD_SENTINEL),
}
SYNTAX_CLASSES = frozenset(("syntax_kwargs", "syntax_star", "syntax_rebinding"))
# An official syntax rejection can end with a generic "syntax error" trailer.
# That trailer is insufficient: a specific expected syntax class must also match.
# Actual parse errors, unexpected tokens and crashes remain forbidden in all cases.
UNRELATED = re.compile(r"parse error|unexpected token|panicked|internal error|FATAL:|crashed|falling back|fallback to", re.I)
SYNTAX_ERROR = re.compile(r"syntax error", re.I)


def digest(data):
    return hashlib.sha256(data).hexdigest()


class RecordingRunner(Runner):
    """Keep exact stdout/stderr digests, including interrupted subprocess runs."""

    def run(self, name, argv, cwd, timeout=None):
        start = len(self.report["runs"])
        try:
            return super().run(name, argv, cwd, timeout)
        finally:
            for record in self.report["runs"][start:]:
                for field in ("stdout", "stderr"):
                    data = (base64.b64decode(record[field + "_base64"]) if field + "_base64" in record
                            else record[field].encode("utf-8"))
                    record[field + "_sha256"] = digest(data)


class DirectReference(Reference):
    def run(self, name, command, *args):
        return super().run(name, command,
                           "--repository_cache=" + str(self.output_base.parent / "repository-cache"),
                           "--repo_contents_cache=" + str(self.output_base.parent / "repository-contents"),
                           "--registry=" + self.registry.as_uri(),
                           "--registry=https://bcr.bazel.build", *args)


def native_command(executable, cache, registry, item):
    return [str(executable), "--ignore_all_rc_files", "--cache-dir=" + str(cache),
            "query", "--lockfile_mode=off", "--registry=" + registry.as_uri(),
            *item["flags"], item["target"]]


def materialize(scratch, workspace, item):
    """Unique registry URI prevents immutable registry MODULE cache cross-talk."""
    registry = scratch / "registry"
    entry = registry / "modules/decldep/1.0"
    source = scratch / "source"
    for directory in (workspace, entry, source):
        directory.mkdir(parents=True, exist_ok=True)
    files = {workspace / "MODULE.bazel": item["root_module"].encode(),
             workspace / "BUILD.bazel": b"",
             workspace / "defs.bzl": item["defs"].encode(),
             registry / "bazel_registry.json": b"{}\n",
             entry.parent / "metadata.json": b'{"versions":["1.0"],"yanked_versions":{}}\n',
             entry / "MODULE.bazel": item["dependency_module"].encode(),
             entry / "source.json": (json.dumps({"type": "local_path", "path": str(source.resolve())}) + "\n").encode(),
             source / "MODULE.bazel": item["dependency_module"].encode(),
             source / "BUILD.bazel": BUILD, source / "data": DATA,
             source / "defs.bzl": item["defs"].encode()}
    for path, data in files.items():
        path.write_bytes(data)
    return registry, files


def snapshot(files):
    return {str(path): file_evidence(path, include_bytes=True) for path in files}


def require_query(record):
    text = require_success(record)
    if (record.get("spawn_error") or record.get("interrupted") or UNRELATED.search(record["stderr"])
            or SYNTAX_ERROR.search(record["stderr"])):
        raise ValueError("query was interrupted, crashed, or used fallback")
    labels = text.splitlines()
    # Same acceptance for both engines: N39 display-form labels plus the N51
    # main mapping make native print the apparent spelling like genuine Bazel.
    if labels not in ([QUERY], [CANONICAL]):
        raise ValueError("query did not return exactly the fixture repository source identity")
    return {"exact_labels": labels, "canonical_identity": CANONICAL}


def require_rejection(record, diagnostic):
    code = record["returncode"]
    if (type(code) is not int or not 0 < code < 128 or code == 101 or record["timed_out"]
            or record.get("spawn_error") or record.get("interrupted") or record["stdout"].strip()
            or UNRELATED.search(record["stderr"])
            or (diagnostic not in SYNTAX_CLASSES and SYNTAX_ERROR.search(record["stderr"]))
            or not re.search(DIAGNOSTICS[diagnostic], record["stderr"], re.I)):
        raise ValueError("expected controlled rejection in diagnostic class " + diagnostic)
    return {"diagnostic_class": diagnostic, "returncode": code}


def evaluate(item, native, official, check):
    expected = item["expected"]
    # Reference errors are never silently counted as semantic agreement.
    try:
        if expected == "rejection":
            check["reference"] = require_rejection(official, item["diagnostic_class"])
        elif expected == "laziness" and official["returncode"] != 0:
            check["reference"] = require_rejection(official, "declaration_loaded")
            check["native"] = require_query(native)
            check["status"] = "UNSUPPORTED_BOUNDARY"
            check["parity_claim"] = False
            check["details"] = "Official query eagerly loaded the declaration fixture; no shared laziness claim."
            return
        else:
            check["reference"] = require_query(official)
    except ValueError:
        check["status"] = "REFERENCE_FAILURE"
        raise
    check["native"] = (require_rejection(native, item["diagnostic_class"]) if expected == "rejection"
                       else require_query(native))
    check["parity_claim"] = True
    check["status"] = "PASS"


def run_case(args, runner, scratch, workspace, reference, item):
    check = {"name": item["name"], "kind": item["kind"], "status": "FAIL", "input": item,
             "expected": item["expected"], "parity_claim": False, "run_ids": {}}
    runner.report["checks"].append(check)
    files = None
    try:
        case_root = scratch / "cases" / item["name"]
        registry, files = materialize(case_root, workspace, item)
        check["files_before"] = snapshot(files)
        check["registry"] = registry.as_uri()
        native = runner.run(item["name"] + ".native", native_command(
            args.native, case_root / "native-cache", registry, item), workspace)
        check["run_ids"]["native"] = native["id"]
        official = None
        if item["kind"] != "native_capability_boundary":
            reference.registry = registry
            official = reference.run(item["name"] + ".reference", "query", *item["flags"], item["target"])
            check["run_ids"]["reference"] = official["id"]
        evaluate(item, native, official, check)
    except (OSError, ValueError) as exc:
        check["error"] = str(exc)
    finally:
        if files is not None:
            try:
                check["files_after"] = snapshot(files)
                check["inputs_unchanged"] = check["files_before"] == check["files_after"]
                if not check["inputs_unchanged"]:
                    check["status"] = "FAIL"
                    check["error"] = "CLI changed fixture input bytes"
            except (OSError, ValueError, KeyError) as exc:
                check["status"] = "FAIL"
                check["error"] = "fixture evidence: " + str(exc)


def report_passes(report):
    """Reject incomplete/malformed reports, not merely non-PASS check statuses."""
    try:
        if ([(c["name"], c["kind"]) for c in report["checks"]] != PLAN
                or report["errors"] or report["scratch_cleaned"] is not True
                or report["reference_shutdown_attempted"] is not True
                or report["sources_unchanged"] is not True
                or report["reference_release"] != "bazel 9.3.0"
                or report["executables"]["reference"]["sha256"] != REFERENCE_SHA256):
            return False
        runs = report["runs"]
        if [run["id"] for run in runs] != list(range(len(runs))):
            return False
        for item, check in zip(CASES, report["checks"]):
            if (check["input"] != item or check["expected"] != item["expected"]
                    or check["inputs_unchanged"] is not True or not check["files_before"]
                    or check["files_before"] != check["files_after"] or check.get("error")):
                return False
            if check["status"] != "PASS":
                if not (item["expected"] == "laziness" and check["status"] == "UNSUPPORTED_BOUNDARY"):
                    return False
            engines = {"native"} if item["kind"] == "native_capability_boundary" else {"native", "reference"}
            if set(check["run_ids"]) != engines:
                return False
            records = {}
            for engine, index in check["run_ids"].items():
                if type(index) is not int or index < 0:
                    return False
                run = runs[index]
                if run["name"] != item["name"] + "." + engine or not run["argv"] or not run["cwd"]:
                    return False
                records[engine] = run
            verified = {}
            evaluate(item, records["native"], records.get("reference"), verified)
            if (verified["status"] != check["status"]
                    or verified["parity_claim"] != check["parity_claim"]):
                return False
        return True
    except (KeyError, TypeError, ValueError, IndexError, AttributeError):
        return False


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", type=Path, default=ROOT / "target/release/bazel-rs")
    parser.add_argument("--reference", type=Path, default=ROOT / ".direnv/reference/bazel-9.3.0-linux-x86_64")
    parser.add_argument("--output", "--report", dest="output", type=Path,
                        default=ROOT / "artifacts/direct-repositories-validation.json")
    parser.add_argument("--scratch-parent", type=Path, default=Path("/tmp/opencode"))
    parser.add_argument("--timeout", type=positive_seconds, default=180)
    return parser


def validate_binaries(args):
    # Both must be direct ELF executables, never a shell wrapper.
    validate_executable(args.native, official=True)
    validate_executable(args.reference, official=True)
    evidence = {name: file_evidence(getattr(args, name)) for name in ("native", "reference")}
    if evidence["reference"]["sha256"] != REFERENCE_SHA256:
        raise ValueError("reference SHA256 differs from pinned official Bazel 9.3.0 Linux x86_64")
    return evidence


def main(argv=None):
    args = argument_parser().parse_args(argv)
    for field in ("native", "reference", "output", "scratch_parent"):
        setattr(args, field, getattr(args, field).expanduser().resolve())
    sources = [Path(__file__).resolve(), ROOT / "scripts/validate.py",
               ROOT / "tests/test_validate_direct_repositories.py"]
    if (args.output in [args.native, args.reference, *sources]
            or args.output.is_relative_to(ROOT / "tests/fixtures")):
        raise ValueError("report must not overwrite executables, harness sources, or tracked fixtures")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {"schema_version": 1, "status": "FAIL", "runs": [], "checks": [], "errors": [], "cleanup": [],
              "scratch_cleaned": False, "reference_shutdown_attempted": False, "sources_unchanged": False,
              "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "configuration": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              "planned_checks": CASES, "case_inputs_sha256": digest(json.dumps(CASES, sort_keys=True).encode()),
              "reference_version_required": "9.3.0", "reference_sha256_required": REFERENCE_SHA256,
              "test_commands_executed": False, "build_commands_executed": False,
              "comparison": "Exact single source-label identity (either display or canonical spelling, "
                          "identically accepted for both engines since N39/N51); controlled diagnostic "
                          "classes, not exact error strings.",
              "limitations": ["MODULE declaration metadata only; full use_repo_rule/Bzlmod parity is NOT attained.",
                              "Nonzero module_extension facts_version is supported since N52 (differential evidence: tests/fixtures/lockfile_extensions/reference_receipt.json); its former native-only boundary probe is retired.",
                              "This suite makes no generated-repository success claim; pure direct execution is validated separately.",
                              "Active-use mod graph is not compared: native mod graph does not execute extensions.",
                              "The load sentinel records official eager loading as UNSUPPORTED_BOUNDARY, never parity.",
                              "Query does not read source contents: before/after fixture bytes are recorded, not build outputs.",
                              "Reference uses real BCR built-ins; no dummy modules, overrides, or cached user repositories.",
                              "Private workspaces, download/content/native caches and output roots; not a network sandbox.",
                              "No wrappers, real ~/code workspace, or credential environment changes."]}
    runner = RecordingRunner(report, args.timeout)
    scratch = reference = None
    interrupted = False
    try:
        report["sources_before"] = snapshot(sources)
        report["executables"] = validate_binaries(args)
        scratch = Path(tempfile.mkdtemp(prefix="direct-repositories-", dir=args.scratch_parent))
        report["scratch_directory"] = str(scratch)
        workspace = scratch / "workspace"
        workspace.mkdir()
        (workspace / "MODULE.bazel").write_text(ROOT_MODULE)
        reference = DirectReference(args.reference, workspace, scratch, runner, "direct-repositories")
        reference.started = True
        version = runner.run("reference.version", reference.startup + ["version", "--gnu_format"], workspace)
        report["reference_release"] = require_success(version).strip()
        if report["reference_release"] != "bazel 9.3.0":
            raise ValueError("reference is not official Bazel 9.3.0")
        for item in CASES:
            run_case(args, runner, scratch, workspace, reference, item)
    except KeyboardInterrupt:
        interrupted = True
        report["errors"].append("interrupted")
    except Exception as exc:
        report["errors"].append(type(exc).__name__ + ": " + str(exc))
    finally:
        if reference is not None:
            try:
                report["reference_shutdown_attempted"] = True
                reference.shutdown()
                report["cleanup"].append({"kind": "reference", "output_base": str(reference.output_base),
                                          "shutdown_attempted": True})
            except Exception as exc:
                report["errors"].append("reference shutdown: " + str(exc))
        if scratch is not None:
            try:
                shutil.rmtree(scratch)
                report["scratch_cleaned"] = not scratch.exists()
                report["cleanup"].append({"kind": "scratch", "path": str(scratch), "removed": report["scratch_cleaned"]})
            except OSError as exc:
                report["errors"].append("scratch cleanup: " + str(exc))
        try:
            report["sources_after"] = snapshot(sources)
            report["sources_unchanged"] = report.get("sources_before") == report["sources_after"]
        except OSError as exc:
            report["errors"].append("source evidence: " + str(exc))
        report["completed_differential_cases"] = sum(c["kind"] == "differential" and c["status"] == "PASS"
                                                      for c in report["checks"])
        if report_passes(report):
            report["status"] = "PASS"
        report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        write_report(args.output, report)
    print("{} (MODULE metadata only; no repository execution parity): {}".format(report["status"], args.output))
    return 130 if interrupted else (0 if report["status"] == "PASS" else 1)


if __name__ == "__main__":
    def terminate(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    sys.exit(main())
