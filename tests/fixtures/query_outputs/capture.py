#!/usr/bin/env python3
"""Capture genuine Bazel 9.3 `query` outputs for tests/query_outputs.rs.

Usage:
  python3 tests/fixtures/query_outputs/capture.py \
    --bazel /home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64 \
    --scratch /home/coder/opencodetmp/w/qout-cap

Copies ws/ to SCRATCH/ws, runs each case of CASES with a private output user
root/base (--batch, no rc files, public BCR for the built-in module closure,
--lockfile_mode=off) and writes genuine/<id>.out (stdout bytes, the scratch
workspace path replaced by ${WORKSPACE}) plus reference_receipt.json (exit
codes, ERROR/WARNING/INFO events, sha256 of the outputs).
"""
import argparse, datetime, hashlib, json, os, pathlib, shutil, subprocess, sys

HERE = pathlib.Path(__file__).resolve().parent

SET = "//pkg:* + //vis:* + //conf:* + //other:*"
REL = ["--relative_locations"]
CASES = [
    # (id, args)
    ("proto.all", ["--output=proto", *REL, SET]),
    ("streamed_proto.all", ["--output=streamed_proto", *REL, SET]),
    ("streamed_jsonproto.all", ["--output=streamed_jsonproto", *REL, SET]),
    ("streamed_jsonproto.absolute", ["--output=streamed_jsonproto", "//pkg:gen + //pkg:data.txt + //pkg:BUILD.bazel"]),
    ("xml.all", ["--output=xml", SET]),
    ("xml.defaults_nolines", ["--output=xml", "--xml:default_values", "--noxml:line_numbers", *REL, SET]),
    ("xml.empty", ["--output=xml", "//pkg:all except //pkg:all"]),
    ("proto.empty", ["--output=proto", "//pkg:all except //pkg:all"]),
    ("proto.noflatten", ["--output=proto", "--noproto:flatten_selects", *REL, "//pkg:selects + //pkg:all_types"]),
    ("jsonproto.noflatten", ["--output=streamed_jsonproto", "--noproto:flatten_selects", *REL, "//pkg:selects"]),
    ("proto.nodefaults", ["--output=proto", "--noproto:default_values", *REL, "//pkg:all"]),
    ("proto.attrs", ["--output=proto", "--proto:output_rule_attrs=s,lbls,tags", *REL, "//pkg:all"]),
    ("proto.attrs_empty", ["--output=proto", "--proto:output_rule_attrs=", "--noproto:rule_inputs_and_outputs", "--noproto:locations", "//pkg:all"]),
    ("proto.stacks", ["--output=streamed_jsonproto", "--proto:instantiation_stack", "--proto:definition_stack", *REL, "//pkg:pair_a + //pkg:gen + //pkg:plain"]),
    ("proto.attr_hash", ["--output=streamed_jsonproto", "--proto:include_synthetic_attribute_hash", *REL, "//pkg:plain + //pkg:gen + //sky:a"]),
    ("proto.notool", ["--output=streamed_jsonproto", "--notool_deps", "--noimplicit_deps", *REL, "//pkg:gen + //sky:c + //pkg:unit_test"]),
    ("proto.deps_order_full", ["--output=streamed_jsonproto", "--order_output=full", "--noproto:default_values", *REL, "deps(//sky:c)"]),
    ("proto.labels_consistent", ["--output=streamed_jsonproto", "--consistent_labels", "--noproto:default_values", *REL, "//pkg:gen"]),
    ("proto.aspects", ["--output=streamed_jsonproto", *REL, "//asp:unmatched + //asp:matched"]),
    ("proto.aspects_off", ["--output=streamed_jsonproto", "--aspect_deps=off", "--noproto:default_values", *REL, "//asp:matched"]),
    ("proto.aspects_source", ["--output=streamed_jsonproto", "--proto:include_attribute_source_aspects", "--noproto:default_values", *REL, "//asp:matched"]),
    ("xml.aspects", ["--output=xml", *REL, "//asp:matched"]),
    ("proto.loads", ["--output=streamed_jsonproto", *REL, "buildfiles(//pkg:all)"]),
    # SkyQuery (--universe_scope / --infer_universe_scope).
    ("sky.allrdeps", ["--universe_scope=//sky/...,//other/...", "allrdeps(//other:thing)"]),
    ("sky.allrdeps_depth", ["--universe_scope=//sky/...,//other/...", "allrdeps(//other:thing, 1)"]),
    ("sky.allrdeps_no", ["--universe_scope=//sky/...,//other/...", "--order_output=no", "allrdeps(//sky:b)"]),
    ("sky.rdeps_universe", ["--universe_scope=//sky/...", "rdeps(//sky/..., //sky:b)"]),
    ("sky.rdeps_sub", ["--universe_scope=//sky/...", "rdeps(//sky:c, //other:thing)"]),
    ("sky.rdeps_notool", ["--universe_scope=//sky/...", "--notool_deps", "rdeps(//sky/..., //sky:b)"]),
    ("sky.infer", ["--infer_universe_scope", "rdeps(//sky/..., //other:thing)"]),
    ("sky.infer_allrdeps", ["--infer_universe_scope", "allrdeps(//sky:b) + //sky:lonely"]),
    ("sky.rbuildfiles", ["--universe_scope=//sky/...,//other/...", "rbuildfiles(rules/common.bzl)"]),
    ("sky.rbuildfiles_macros", ["--universe_scope=//sky/...", "rbuildfiles(rules/macros.bzl, other/BUILD.bazel)"]),
    ("sky.rbuildfiles_none", ["--universe_scope=//sky/...", "rbuildfiles(nope.bzl)"]),
    ("sky.deps", ["--universe_scope=//sky/...", "deps(//sky:c)"]),
    ("sky.outside", ["--universe_scope=//other/...", "deps(//sky:c)"]),
    ("sky.outside_allrdeps", ["--universe_scope=//other/...", "allrdeps(//sky:c)"]),
    ("sky.label_kind", ["--universe_scope=//sky/...", "--output=label_kind", "allrdeps(//other:thing)"]),
    ("sky.proto", ["--universe_scope=//sky/...", "--output=streamed_jsonproto", "--noproto:default_values", *REL, "allrdeps(//sky:b)"]),
    ("sky.full_falls_back", ["--universe_scope=//sky/...", "--order_output=full", "allrdeps(//sky:b)"]),
    ("sky.graph_falls_back", ["--universe_scope=//sky/...", "--output=graph", "rdeps(//sky/..., //sky:b)"]),
    ("sky.no_universe_allrdeps", ["allrdeps(//sky:b)"]),
    ("sky.no_universe_rbuildfiles", ["rbuildfiles(rules/common.bzl)"]),
    ("sky.bad_universe", ["--universe_scope=//nope/...", "allrdeps(//sky:b)"]),
    ("sky.unknown_function", ["--universe_scope=//sky/...", "foo(//sky:a)"]),
    ("sky.keep_going_outside", ["--universe_scope=//other/...", "--keep_going", "//sky:c + //other:thing"]),
    ("sky.siblings_buildfiles", ["--universe_scope=//sky/...", "buildfiles(//sky:a) + siblings(//sky:a)"]),
]


def events(stderr):
    out = []
    for line in stderr.splitlines():
        if line.startswith(("ERROR:", "WARNING:", "INFO:")) and "Invocation ID" not in line:
            out.append(line)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bazel", required=True)
    ap.add_argument("--scratch", required=True)
    ap.add_argument("--only", nargs="*")
    args = ap.parse_args()
    scratch = pathlib.Path(args.scratch).resolve()
    ws = scratch / "ws"
    if ws.exists():
        shutil.rmtree(ws)
    shutil.copytree(HERE / "ws", ws)
    out_dir = HERE / "genuine"
    out_dir.mkdir(exist_ok=True)
    receipt_path = HERE / "reference_receipt.json"
    receipt = json.loads(receipt_path.read_text()) if receipt_path.exists() and args.only else {"cases": {}}
    prefix = [args.bazel, "--batch", "--ignore_all_rc_files", f"--output_user_root={scratch / 'our'}",
              f"--output_base={scratch / 'ob'}", "--host_jvm_args=-Xmx1g", "query", "--lockfile_mode=off",
              "--color=no", "--curses=no", "--noshow_progress"]
    for case_id, case_args in CASES:
        if args.only and case_id not in args.only:
            continue
        proc = subprocess.run(prefix + case_args, cwd=ws, capture_output=True, timeout=600)
        stdout = proc.stdout.replace(str(ws).encode(), b"${WORKSPACE}")
        (out_dir / f"{case_id}.out").write_bytes(stdout)
        stderr = proc.stderr.decode("utf-8", "replace").replace(str(ws), "${WORKSPACE}")
        receipt["cases"][case_id] = {
            "args": case_args,
            "exit_code": proc.returncode,
            "events": events(stderr),
            "stdout_sha256": hashlib.sha256(stdout).hexdigest(),
            "stdout_bytes": len(stdout),
        }
        print(case_id, proc.returncode, len(stdout), events(stderr)[:3], flush=True)
    receipt["bazel"] = args.bazel
    receipt["bazel_sha256"] = hashlib.sha256(pathlib.Path(args.bazel).read_bytes()).hexdigest()
    receipt["captured"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    receipt["fixture_sha256"] = {
        str(p.relative_to(HERE / "ws")): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in sorted((HERE / "ws").rglob("*")) if p.is_file()
    }
    receipt["network"] = "public BCR (https://bcr.bazel.build) for the built-in module closure; no credentials"
    receipt_path.write_text(json.dumps(receipt, indent=1, sort_keys=True) + "\n")


if __name__ == "__main__":
    sys.exit(main())
