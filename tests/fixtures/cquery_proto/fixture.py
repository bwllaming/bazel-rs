#!/usr/bin/env python3
"""Capture offline Bazel 9.3 output, generate typed DTOs, or independently decode.

Requires google.protobuf (and grpc_tools only for capture). Nothing is installed
by this script. Never points Bazel at a developer's repository or starts a server.
Generated .rs is fixture data, not a second production encoder/decoder.
"""

import argparse
import functools
import hashlib
import json
import pathlib
import shutil
import subprocess
import sys

from google.protobuf import descriptor_pb2, descriptor_pool, message_factory


HERE = pathlib.Path(__file__).resolve().parent
QUERY = "deps(set(//:main //:made.txt //:group //:stamp_auto //:stamp_no //:stamp_yes))"


@functools.lru_cache(maxsize=1)
def classes():
    pool = descriptor_pool.DescriptorPool()
    descriptors = descriptor_pb2.FileDescriptorSet.FromString((HERE / "schema.pb").read_bytes())
    for descriptor in descriptors.file:
        pool.Add(descriptor)
    return [message_factory.GetMessageClass(pool.FindMessageTypeByName(name)) for name in
            ("analysis.CqueryResult", "blaze_query.QueryResult")]


def varint(value):
    data = bytearray()
    while value >= 128:
        data.append((value & 127) | 128)
        value >>= 7
    data.append(value)
    return data


def decode(path, streamed=False, configurations=True):
    cls = classes()[0 if configurations else 1]
    data = pathlib.Path(path).read_bytes()
    result = cls()
    if not streamed:
        result.ParseFromString(data)
    else:
        pos = 0
        while pos < len(data):
            length, shift = 0, 0
            while True:
                assert pos < len(data) and shift <= 63, "invalid frame prefix"
                byte = data[pos]
                pos += 1
                length |= (byte & 127) << shift
                if byte < 128:
                    break
                shift += 7
            assert pos + length <= len(data), "truncated frame"
            record = cls.FromString(data[pos:pos + length])
            pos += length
            if configurations:
                assert len(record.results) + len(record.configurations) == 1
            else:
                assert len(record.target) == 1
            result.MergeFrom(record)
    assert result.IsInitialized(), result.FindInitializationErrors()
    return result


def identity(target):
    payload = (target.rule, target.source_file, target.generated_file, target.package_group)
    return next(p.name for p in payload if p.HasField("name"))


def normalized(result):
    """Normalize only output-local IDs and graph record/edge order, NOT attrs/lists."""
    normalized_result = type(result)()
    normalized_result.CopyFrom(result)
    configs = sorted(normalized_result.configurations, key=lambda c: c.checksum)
    ids = {config.id: index + 1 for index, config in enumerate(configs)}
    for config in configs:
        config.id = ids[config.id]
    del normalized_result.configurations[:]
    normalized_result.configurations.extend(configs)
    for target in normalized_result.results:
        target.configuration_id = ids.get(target.configuration_id, 0)
        rule = target.target.rule
        for edge in rule.configured_rule_input:
            if edge.HasField("configuration_id"):
                edge.configuration_id = ids[edge.configuration_id]
        edges = sorted(rule.configured_rule_input,
                       key=lambda e: (e.label, e.configuration_checksum))
        del rule.configured_rule_input[:]
        rule.configured_rule_input.extend(edges)
    targets = sorted(normalized_result.results,
                     key=lambda t: (identity(t.target), t.configuration.checksum))
    del normalized_result.results[:]
    normalized_result.results.extend(targets)
    return normalized_result


def normalized_query(result):
    copy = type(result)()
    copy.CopyFrom(result)
    for target in copy.target:
        if not target.HasField("rule"):
            continue
        for edge in target.rule.configured_rule_input:
            edge.ClearField("configuration_id")
        edges = sorted(target.rule.configured_rule_input,
                       key=lambda e: (e.label, e.configuration_checksum))
        del target.rule.configured_rule_input[:]
        target.rule.configured_rule_input.extend(edges)
    targets = sorted(copy.target, key=identity)
    del copy.target[:]
    copy.target.extend(targets)
    return copy


def assert_contract(result):
    targets = {identity(t.target): t for t in result.results}
    main = targets["//:main"].target.rule
    attrs = {a.name: a for a in main.attribute}
    assert list(attrs["srcs"].string_list_value) == ["//:selected.txt", "//:source.txt"]
    assert "//:unselected.txt" not in targets
    assert attrs["integer"].int_value == -2147483648
    assert list(attrs["integers"].int_list_value) == [-1, 0, 2147483647]
    assert attrs["text"].string_value == "héllo 世界\n"
    assert list(attrs["texts"].string_list_value) == ["z", "", "a", "z"]
    assert [(v.key, v.value) for v in attrs["dict"].string_dict_value] == [
        ("z", ""), ("", "零"), ("a", "last")]
    assert attrs["labels_dict"].type == 12
    assert not attrs["labels_dict"].label_list_dict_value
    assert len(attrs["labels_dict"].string_list_dict_value) == 2
    for name, explicit in [("enabled", True), ("default_enabled", False)]:
        attr = attrs[name]
        assert attr.HasField("boolean_value") and not attr.boolean_value
        assert attr.HasField("int_value") and attr.int_value == 0
        assert attr.string_value == "false"
        assert attr.HasField("explicitly_specified") and attr.explicitly_specified == explicit
    assert not attrs["unset_dep"].HasField("string_value")
    assert attrs["$rule_implementation_hash"].string_value
    assert not attrs["$rule_implementation_hash"].HasField("explicitly_specified")
    for name, enum, legacy, text in [("auto", 2, -1, "auto"), ("no", 0, 0, "no"),
                                     ("yes", 1, 1, "yes")]:
        stamp = next(a for a in targets[f"//:stamp_{name}"].target.rule.attribute
                     if a.name == "stamp")
        assert stamp.type == 15 and stamp.tristate_value == enum
        assert stamp.int_value == legacy and stamp.string_value == text
    assert targets["//:alias"].target.rule.rule_class == "alias"
    assert targets["//:made.txt"].target.generated_file.generating_rule == "//:main"
    assert targets["//:group"].target.type == 4
    for target in result.results:
        assert target.HasField("configuration")
        if target.target.type in (2, 4):
            assert target.configuration.checksum == "null" and target.configuration_id == 0
        else:
            assert len(target.configuration.checksum) == 64
        assert target.configuration.id == 0 and not target.configuration.mnemonic
        for edge in target.target.rule.configured_rule_input:
            if edge.configuration_checksum:
                assert edge.configuration_id > 0
            else:
                assert not edge.HasField("configuration_checksum")
                assert not edge.HasField("configuration_id")
    assert any(a.source_aspect_name for a in main.attribute), "missing aspect fixture"


def rs_string(value):
    # JSON quoting is used solely to produce a Rust source literal, never a cache.
    return json.dumps(value, ensure_ascii=False) + ".into()"


def rs_bool(value):
    return "true" if value else "false"


def vec(values, render=rs_string):
    return "vec![" + ", ".join(render(value) for value in values) + "]"


def optional(message, field, render=rs_string):
    return f"Some({render(getattr(message, field))})" if message.HasField(field) else "None"


def struct(type_name, **fields):
    return type_name + " { " + ", ".join(f"{key}: {value}" for key, value in fields.items()) + " }"


def license_value(value):
    return struct("License", license_types=vec(value.license_type), exceptions=vec(value.exception))


def attribute(attr):
    types = {1: ("Integer", "int_value", str), 2: ("String", "string_value", rs_string),
             3: ("Label", "string_value", rs_string), 4: ("Output", "string_value", rs_string),
             5: ("StringList", "string_list_value", vec),
             6: ("LabelList", "string_list_value", vec),
             7: ("OutputList", "string_list_value", vec),
             8: ("DistributionSet", "string_list_value", vec),
             9: ("License", "license", license_value),
             10: ("StringDict", "string_dict_value", None),
             12: ("LabelListDict", "string_list_dict_value", None),
             13: ("StringListDict", "string_list_dict_value", None),
             14: ("Boolean", "boolean_value", rs_bool),
             15: ("Tristate", "tristate_value", lambda v: ["Tristate::No", "Tristate::Yes", "Tristate::Auto"][v]),
             16: ("IntegerList", "int_list_value", lambda v: vec(v, str)),
             19: ("LabelDictUnary", "label_dict_unary_value", None),
             21: ("LabelKeyedStringDict", "label_keyed_string_dict_value", None)}
    name, field, render = types[attr.type]
    if attr.DESCRIPTOR.fields_by_name[field].is_repeated:
        value = getattr(attr, field)
        if render is None:
            render_value = vec if attr.type in (12, 13) else rs_string
            render = lambda v: vec(v, lambda e: f"({rs_string(e.key)}, {render_value(e.value)})")
        value = f"AttributeValue::{name}({render(value)})"
    elif attr.HasField(field):
        value = f"AttributeValue::{name}({render(getattr(attr, field))})"
    else:
        value = f"AttributeValue::Unset(AttributeType::{name})"
    return struct("Attribute", name=rs_string(attr.name), value=value,
                  explicitly_specified=optional(attr, "explicitly_specified", rs_bool),
                  nodep=optional(attr, "nodep", rs_bool),
                  source_aspect_name=optional(attr, "source_aspect_name"))


def edge(value):
    return struct("ConfiguredRuleInput", label=rs_string(value.label),
                  configuration_checksum=optional(value, "configuration_checksum"),
                  configuration_id=optional(value, "configuration_id", str))


def target(value):
    if value.type == 1:
        r = value.rule
        body = struct("Rule", name=rs_string(r.name), rule_class=rs_string(r.rule_class),
                      location=optional(r, "location"), attributes=vec(r.attribute, attribute),
                      rule_inputs=vec(r.rule_input), rule_outputs=vec(r.rule_output),
                      default_settings=vec(r.default_setting),
                      configured_rule_inputs=vec(r.configured_rule_input, edge),
                      instantiation_stack=vec(r.instantiation_stack), definition_stack=vec(r.definition_stack))
        return f"Target::Rule({body})"
    if value.type == 2:
        f = value.source_file
        body = struct("SourceFile", name=rs_string(f.name), location=optional(f, "location"),
                      subincludes=vec(f.subinclude), package_groups=vec(f.package_group),
                      visibility_labels=vec(f.visibility_label), features=vec(f.feature),
                      license=optional(f, "license", license_value),
                      package_contains_errors=optional(f, "package_contains_errors", rs_bool))
        return f"Target::SourceFile({body})"
    if value.type == 3:
        f = value.generated_file
        return "Target::GeneratedFile(" + struct("GeneratedFile", name=rs_string(f.name),
                generating_rule=rs_string(f.generating_rule), location=optional(f, "location")) + ")"
    assert value.type == 4
    g = value.package_group
    return "Target::PackageGroup(" + struct("PackageGroup", name=rs_string(g.name),
            contained_packages=vec(g.contained_package), included_package_groups=vec(g.included_package_group)) + ")"


def configuration(c):
    return struct("Configuration", id=str(c.id), checksum=rs_string(c.checksum),
                  mnemonic=rs_string(c.mnemonic), platform_name=rs_string(c.platform_name),
                  is_tool=rs_bool(c.is_tool),
                  fragments=vec(c.fragments, lambda f: struct("Fragment", name=rs_string(f.name),
                                fragment_option_names=vec(f.fragment_option_names))),
                  fragment_options=vec(c.fragment_options, lambda f: struct("FragmentOptions",
                      name=rs_string(f.name), options=vec(f.options, lambda o: struct("ConfigurationOption",
                      name=optional(o, "name"), value=optional(o, "value"))))))


def generate():
    result = decode(HERE / "genuine.proto")
    assert_contract(result)
    assert normalized(result) == normalized(decode(HERE / "genuine.streamed_proto", True))
    targets = vec(result.results, lambda t: f"({target(t.target)}, {t.configuration_id})")
    configs = vec(result.configurations, configuration)
    (HERE / "captured.rs").write_text(
        "// Generated by fixture.py using upstream Bazel 9.3 descriptors; do not hand edit.\n"
        "use bazel_rs::cquery_proto::*;\n\n#[rustfmt::skip]\n"
        "pub fn captured() -> (Vec<(Target, u32)>, Vec<Configuration>) {\n"
        f"    ({targets}, {configs})\n}}\n", encoding="utf-8")
    framed = bytearray()
    for target_record in result.results:
        single = type(result)()
        single.results.append(target_record)
        data = single.SerializeToString(deterministic=True)
        framed.extend(varint(len(data)))
        framed.extend(data)
    for config in result.configurations:
        single = type(result)()
        single.configurations.append(config)
        data = single.SerializeToString(deterministic=True)
        framed.extend(varint(len(data)))
        framed.extend(data)
    (HERE / "canonical.streamed_proto").write_bytes(framed)
    query_result = classes()[1]()
    for t in result.results:
        query_result.target.append(t.target)
    (HERE / "canonical.no_config.proto").write_bytes(query_result.SerializeToString(deterministic=True))
    framed.clear()
    for t in query_result.target:
        single = type(query_result)()
        single.target.append(t)
        data = single.SerializeToString(deterministic=True)
        framed.extend(varint(len(data)))
        framed.extend(data)
    (HERE / "canonical.no_config.streamed_proto").write_bytes(framed)
    # Genuine no-config output confirms the wrapper shape and retained edge IDs.
    unconfigured = decode(HERE / "genuine.no_config.streamed_proto", True, False)
    assert normalized_query(query_result) == normalized_query(unconfigured)
    assert any(e.HasField("configuration_id") for t in unconfigured.target for e in t.rule.configured_rule_input)
    print(f"Validated {len(result.results)} targets, {len(result.configurations)} configs against upstream schema")


def capture(args):
    scratch = pathlib.Path(args.scratch).resolve()
    assert scratch != HERE and HERE not in scratch.parents
    scratch.mkdir(parents=True, exist_ok=True)
    workspace = scratch / "workspace"
    shutil.copytree(HERE / "workspace", workspace, dirs_exist_ok=True)
    for name in ("home", "tmp"):
        (scratch / name).mkdir(exist_ok=True)
    reference = pathlib.Path(args.reference).resolve()
    subprocess.run([sys.executable, "-m", "grpc_tools.protoc", f"-I{reference}",
                    f"--descriptor_set_out={HERE / 'schema.pb'}", "--include_imports",
                    str(reference / "src/main/protobuf/analysis_v2.proto")], check=True)
    common = [args.bazel, "--ignore_all_rc_files", "--batch",
              f"--output_user_root={scratch / 'userroot'}", f"--output_base={scratch / 'outputbase'}",
              "cquery", QUERY, "--incompatible_autoload_externally=", "--lockfile_mode=off",
              "--repository_disable_download", "--transitions=lite",
              "--proto:include_attribute_source_aspects"]
    for filename, output, extra in [("genuine.proto", "proto", []),
                                     ("genuine.streamed_proto", "streamed_proto", []),
                                     ("genuine.no_config.streamed_proto", "streamed_proto",
                                      ["--noproto:include_configurations"])]:
        with (scratch / (filename + ".log")).open("wb") as log:
            subprocess.run(common + [f"--output={output}", f"--output_file={HERE / filename}"] + extra,
                           cwd=workspace, env={"PATH": "/usr/bin:/bin", "HOME": str(scratch / "home"),
                                               "TMPDIR": str(scratch / "tmp")},
                           stdout=log, stderr=log, check=True)
    sources = [reference / f"src/main/protobuf/{name}.proto"
               for name in ("analysis_v2", "build", "stardoc_output")]
    sources += [pathlib.Path(args.bazel)]
    (HERE / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n" for p in sources))
    generate()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    cap = sub.add_parser("capture")
    cap.add_argument("--scratch", required=True)
    cap.add_argument("--reference", required=True)
    cap.add_argument("--bazel", required=True)
    sub.add_parser("generate")
    check = sub.add_parser("check")
    check.add_argument("path")
    check.add_argument("--streamed", action="store_true")
    args = parser.parse_args()
    if args.command == "capture":
        capture(args)
    elif args.command == "generate":
        generate()
    else:
        result = decode(args.path, args.streamed)
        assert_contract(result)
        assert normalized(result) == normalized(decode(HERE / "genuine.proto"))
        print("Independent upstream-schema decode: semantic equality, required fields and null configs OK")


if __name__ == "__main__":
    main()
