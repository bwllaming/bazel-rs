//! Bounded native JSON, exercised through the real CLI and action bytes.
//! Saved genuine fixtures are portable; no ignored research artifact is needed.
//! Native depth/resource and MAIN File configuration are declared scope limits,
//! not JVM stack/RSS/performance, external File, or nondefault UTF16 parity claims.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(defs: &str, build: &str) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("json-cli-")
                .tempdir()
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='json_fixture')\n");
        ws.file("defs.bzl", defs);
        ws.file("BUILD.bazel", build);
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: impl AsRef<[u8]>) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn fixture(fixture: &serde_json::Value) -> Self {
        let ws = Self::new("", "");
        for (name, text) in fixture["files"].as_object().unwrap() {
            ws.file(name, text.as_str().unwrap());
        }
        ws
    }

    fn run(&self, args: &[&str]) -> Output {
        let home = self.path().join("private-home");
        let tmp = self.path().join("private-tmp");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&tmp).unwrap();
        let output = Command::new("timeout")
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home)
            .env("TMPDIR", tmp)
            .env("LC_ALL", "C")
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.path().join("private-cache").display()
            ))
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert!(
            !matches!(output.status.code(), Some(124 | 137)),
            "timeout/killed: {}",
            errors(&output)
        );
        output
    }

    fn untouched(&self) {
        for name in [
            "executed-marker",
            "private-cache/actions",
            "private-cache/work",
            "bazel-rs-out",
        ] {
            assert!(!self.path().join(name).exists(), "unexpected {name}");
        }
    }

    fn check(body: &str) -> Self {
        Self::new(&rule(body), CHECK)
    }

    fn rejects(body: &str, diagnostic: &str) -> Output {
        let ws = Self::check(&format!(
            "out=ctx.actions.declare_file('registered.out')\nctx.actions.write(out,'must not execute')\nprint('JSON_CALL_ENTER')\n{body}\nfail('GUARD_BYPASSED')"
        ));
        success(&ws.run(&["query", ":check"]));
        let build = format!(
            "{CHECK}genrule(name='first',outs=['first.out'],cmd=\"touch '{}'; printf first > $@\")\n",
            ws.path().join("executed-marker").display()
        );
        ws.file("BUILD.bazel", build);
        let output = ws.run(&["build", ":first", ":check"]);
        rejected(&output, diagnostic);
        assert!(
            errors(&output).contains("JSON_CALL_ENTER"),
            "{}",
            errors(&output)
        );
        ws.untouched();
        output
    }
}

const CHECK: &str = "load(':defs.bzl','check')\ncheck(name='check')\n";

fn rule(body: &str) -> String {
    let body = body
        .lines()
        .map(|s| format!("    {s}\n"))
        .collect::<String>();
    format!(
        "def _check(ctx):\n    text=''\n{body}    out=ctx.actions.declare_file('check.out')\n    ctx.actions.write(out,text)\n    return [DefaultInfo(files=depset([out]))]\ncheck=rule(implementation=_check)\n"
    )
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn success(output: &Output) {
    assert!(output.status.success(), "{}", errors(output));
}
fn rejected(output: &Output, diagnostic: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", errors(output));
    assert!(output.stdout.is_empty(), "partial stdout");
    assert!(
        errors(output).contains(diagnostic),
        "expected {diagnostic:?}: {}",
        errors(output)
    );
    for bad in [
        "panicked",
        "GUARD_BYPASSED",
        "Variable `json` not found",
        "name 'json' is not defined",
    ] {
        assert!(!errors(output).contains(bad), "{}", errors(output));
    }
}
fn paths(output: &Output) -> Vec<PathBuf> {
    success(output);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| PathBuf::from(s.split_once('\t').expect("native action output path").1))
        .collect()
}
fn content(output: &Output) -> Vec<Vec<u8>> {
    paths(output).iter().map(|p| fs::read(p).unwrap()).collect()
}
fn named_content(output: &Output) -> BTreeMap<String, Vec<u8>> {
    paths(output)
        .iter()
        .map(|p| {
            (
                p.file_name().unwrap().to_str().unwrap().to_owned(),
                fs::read(p).unwrap(),
            )
        })
        .collect()
}
fn counts(output: &Output, executed: usize, hits: usize) {
    success(output);
    assert!(
        errors(output).contains(&format!("{executed} actions executed, {hits} cache hits")),
        "{}",
        errors(output)
    );
}
fn expect(body: &str, expected: &str) {
    let ws = Workspace::check(body);
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [expected.as_bytes().to_vec()]
    );
}

#[test]
fn corrected_genuine_provider_consumer_nineteen_outputs_cold_warm_source_change() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/json/functional_reference.json")).unwrap();
    assert_eq!(fixture["name"], "corrected-string-source-functional");
    assert_eq!(fixture["provenance"]["run_index"], 38);
    let ws = Workspace::fixture(&fixture);
    // Native BUILD has no print global. Preserve the saved genuine source and
    // evaluate exactly the same JSON expressions without this unrelated print.
    let build = fixture["files"]["BUILD.bazel"]
        .as_str()
        .unwrap()
        .replace("print(\"BUILD_META\",", "BUILD_META=(");
    ws.file("BUILD.bazel", &build);
    let expected: BTreeMap<_, _> = fixture["outputs"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().as_bytes().to_vec()))
        .collect();
    assert_eq!(expected.len(), 19);
    let cold = ws.run(&["build", ":check"]);
    assert_eq!(named_content(&cold), expected);
    counts(&cold, 19, 0);
    let warm = ws.run(&["build", ":check"]);
    assert_eq!(named_content(&warm), expected);
    counts(&warm, 0, 19);
    assert_eq!(paths(&cold), paths(&warm));
    ws.file(
        "BUILD.bazel",
        build.replace("\"source\":\"dependency\"", "\"source\":\"changed\""),
    );
    let changed = ws.run(&["build", ":check"]);
    let mut changed_expected = expected;
    changed_expected.insert(
        "dependency.json".into(),
        br#"{"a":{"source":"changed"},"z":[1,2]}"#.to_vec(),
    );
    assert_eq!(named_content(&changed), changed_expected);
    counts(&changed, 1, 18);
}

#[test]
fn build_bzl_module_metadata_fresh_methods_and_frozen_alias_identity() {
    let defs = format!(
        "Saved=json.encode\nAlias=Saved\nModule=json\n{}",
        rule(
            r#"
if type(json)!='json' or dir(json)!=['decode','encode','encode_indent','indent']:
    fail('module metadata')
if repr(json)!='<unknown object net.starlark.java.lib.json.Json>' or str(json)!=repr(json):
    fail('module repr')
if json!=Module or Saved!=Alias or json.encode==json.encode or Saved==json.encode:
    fail('method identity')
for name in dir(json):
    method=getattr(json,name)
    if type(method)!='builtin_function_or_method' or repr(method)!='<built-in method '+name+' of json value>':
        fail('method metadata')
text=Saved({'ok':True})
"#
        )
    );
    let ws = Workspace::new(
        &defs,
        &format!("{CHECK}BUILD_VALUE=json.encode(json.decode('[true,null]'))\n"),
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [br#"{"ok":true}"#.to_vec()]
    );
}

#[test]
fn root_module_json_is_undefined_even_for_direct_local_query() {
    let ws = Workspace::new("", "filegroup(name='check')\n");
    ws.file("MODULE.bazel", "module(name='json_fixture')\nx=json\n");
    let output = ws.run(&["query", ":check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        errors(&output).contains("name 'json' is not defined")
            && errors(&output).contains("MODULE.bazel"),
        "{}",
        errors(&output)
    );
    assert!(output.stdout.is_empty());
    ws.untouched();
}

#[test]
fn module_loaded_bzl_repository_callback_mutates_and_rebuilds_actual_input() {
    let ws = Workspace::new(
        "",
        "genrule(name='check',srcs=['@jsonrepo//:result.json'],outs=['check.out'],cmd='cat $(SRCS) > $@')\n",
    );
    let module = "module(name='json_fixture')\nr=use_repo_rule('//:repo.bzl','r')\nr(name='jsonrepo',data='{\"z\":0,\"a\":[1]}')\n";
    ws.file("MODULE.bazel", module);
    ws.file(
        "repo.bzl",
        r#"
MODULE_JSON_TYPE=type(json)
def _repo(ctx):
    if MODULE_JSON_TYPE!='json':
        fail('module bzl missing json')
    d=json.decode(ctx.attr.data)
    d['callback']=True
    ctx.file('result.json',json.encode(d),executable=False)
    ctx.file('BUILD.bazel','exports_files(["result.json"])')
r=repository_rule(implementation=_repo,attrs={'data':attr.string()})
"#,
    );
    let cold = ws.run(&["build", ":check"]);
    assert_eq!(
        content(&cold),
        [br#"{"a":[1],"callback":true,"z":0}"#.to_vec()]
    );
    counts(&cold, 1, 0);
    let warm = ws.run(&["build", ":check"]);
    assert_eq!(content(&warm), content(&cold));
    counts(&warm, 0, 1);
    ws.file("MODULE.bazel", module.replace("[1]", "[2]"));
    let changed = ws.run(&["build", ":check"]);
    assert_eq!(
        content(&changed),
        [br#"{"a":[2],"callback":true,"z":0}"#.to_vec()]
    );
    counts(&changed, 1, 0);
}

#[test]
fn json_and_methods_unhashable_immutable_and_not_native_aliases() {
    for body in ["x={json:1}", "x={json.encode:1}", "x=set([json.decode])"] {
        Workspace::rejects(body, "hash");
    }
    // EvalUtils.setField (EvalUtils.java:526-535): json is not a Structure.
    Workspace::rejects("json.encode=17", "cannot set .encode field of json value");
    Workspace::rejects("x=native.json", "json");
}

#[test]
fn binding_positional_primary_keyword_only_formatting_and_type_before_fallback() {
    for call in [
        "json.encode()",
        "json.encode(x=1)",
        "json.encode(1,2)",
        "json.decode()",
        "json.decode(x='null')",
        "json.decode('!',1,2)",
        "json.decode('!',1,default=2)",
        "json.decode(17,default='FALLBACK_RETURNED')",
        "json.indent(s='[]')",
        "json.encode_indent(x=[])",
        "json.indent('[]','p')",
        "json.encode_indent([],'p')",
        "json.indent('[]',prefix=1)",
        "json.indent('[]',indent=None)",
        "json.encode_indent([],prefix=1)",
        "json.encode_indent([],indent=False)",
    ] {
        // Diagnostics differ by static versus runtime binding; reaching this
        // callback then failing without fallback/partial actions is the contract.
        Workspace::rejects(&format!("x={call}"), "");
    }
    expect(
        "fallback=[]\nx=json.decode('!',fallback)\nx.append(1)\ny=json.decode('!',default=fallback)\ny.append(2)\ntext=json.encode([fallback,json.decode('null',fallback),json.decode('!',None)])",
        "[[1,2],null,null]",
    );
}

#[test]
fn integer_float_lexical_types_exact_bigint_bytes_negative_zero_and_overflow() {
    expect(
        r#"
tokens=['18446744073709551616','-18446744073709551617','-0','-0.0','1e3','5.E1']
values=[json.decode(s) for s in tokens]
if [type(v) for v in values]!=['int','int','int','float','float','float']:
    fail('lexical number types')
if type(json.decode('1e999'))!='float' or type(json.decode('-1e999'))!='float':
    fail('overflow must remain a float')
if json.decode('5.e1',default='bad')!='bad':
    fail('pinned lowercase exponent quirk')
text=json.encode(values)
"#,
        "[18446744073709551616,-18446744073709551617,0,-0.0,1000.0,50.0]",
    );
    for token in ["1e999", "-1e999"] {
        Workspace::rejects(&format!("x=json.encode(json.decode('{token}'))"), "finite");
    }
    Workspace::rejects("inf=json.decode('1e999')\nx=json.encode(inf-inf)", "finite");
}

#[test]
fn all_1024_genuine_float_strings_through_real_json_action_and_binary64_bits() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/json/float_reference.json")).unwrap();
    let rows = fixture["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1024);
    let mut lexical = Vec::new();
    let mut expected = Vec::new();
    for row in rows {
        let bits = u64::from_str_radix(row["bits_hex"].as_str().unwrap(), 16).unwrap();
        let input = row["input_decimal"].as_str().unwrap();
        assert_eq!(input.parse::<f64>().unwrap().to_bits(), bits);
        // Native globals need not expose float(). Force the genuine input
        // decimal's lexical class to float, preserving -0 as -0.0.
        lexical.push(if input.contains(['.', 'e', 'E']) {
            input.to_owned()
        } else {
            format!("{input}.0")
        });
        expected.push(row["expected_json"].as_str().unwrap().to_owned());
    }
    let source = serde_json::to_string(&lexical).unwrap();
    let ws = Workspace::new(
        &format!(
            "INPUTS={source}\n{}",
            rule(
                r#"
values=[json.decode(s) for s in INPUTS]
if len(values)!=1024 or any([type(v)!='float' for v in values]):
    fail('float lexical class lost')
# One user JSON call encodes ALL floats: exercise per-call Math reuse and the
# cumulative budget. Splitting scalar numeric tokens needs no JSON substitute.
encoded=json.encode(values)
strings=encoded[1:-1].split(',')
if len(strings)!=1024:
    fail('float output count')
text=json.encode(strings)
"#
            )
        ),
        CHECK,
    );
    let output = ws.run(&["build", ":check"]);
    let bytes = content(&output);
    assert_eq!(bytes.len(), 1);
    let actual: Vec<String> = serde_json::from_slice(&bytes[0]).unwrap();
    assert_eq!(actual.len(), 1024);
    for (index, ((got, want), row)) in actual.iter().zip(&expected).zip(rows).enumerate() {
        assert_eq!(
            got.as_bytes(),
            want.as_bytes(),
            "row {index}, bits {}",
            row["bits_hex"]
        );
        assert_eq!(
            got.parse::<f64>().unwrap().to_bits(),
            u64::from_str_radix(row["bits_hex"].as_str().unwrap(), 16).unwrap(),
            "row {index}"
        );
    }
    counts(&output, 1, 0);
}

#[test]
fn duplicate_first_position_last_value_fresh_nested_mutable_caller_heap() {
    expect(
        r#"
a=json.decode('{"z":1,"a":2,"z":3,"nested":[{}]}')
b=json.decode('{"z":1,"a":2,"z":3,"nested":[{}]}')
if a.keys()!=['z','a','nested']:
    fail('duplicate insertion order')
a['nested'][0]['x']=1
json.encode(a)
json.encode_indent(a)
a['nested'].append(2)
text=json.encode([a,b])
"#,
        r#"[{"a":2,"nested":[{"x":1},2],"z":3},{"a":2,"nested":[{}],"z":3}]"#,
    );
}

#[test]
fn frozen_module_results_remain_frozen_but_saved_decoder_allocates_in_caller() {
    let prefix = "FROZEN=json.decode('{\"a\":[{}]}')\nSaved=json.decode\n";
    let ws = Workspace::new(
        &format!(
            "{prefix}{}",
            rule("d=Saved('{\"a\":[{}]}')\nd['a'][0]['caller']=True\ntext=json.encode([FROZEN,d])")
        ),
        CHECK,
    );
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [br#"[{"a":[{}]},{"a":[{"caller":true}]}]"#.to_vec()]
    );
    let ws = Workspace::new(
        &format!(
            "{prefix}{}",
            rule("FROZEN['a'].append(1)\ntext='GUARD_BYPASSED'")
        ),
        CHECK,
    );
    let output = ws.run(&["build", ":check"]);
    rejected(&output, "");
    assert!(
        errors(&output).to_lowercase().contains("frozen")
            || errors(&output).to_lowercase().contains("immutable"),
        "{}",
        errors(&output)
    );
    ws.untouched();
}

#[test]
fn json_controls_unicode_surrogates_are_action_bytes_not_outer_repr() {
    expect(
        r#"text=json.encode(["\000\001\b\f\n\r\t\"\\/",'é😀\177',json.decode('"\\u00e9\\ud83d\\ude00"'),json.decode('"\\ud800X\\udc00"')])"#,
        "[\"\\u0000\\u0001\\b\\f\\n\\r\\t\\\"\\\\/\",\"é😀\u{7f}\",\"é😀\",\"�X�\"]",
    );
}

#[test]
fn decode_error_offsets_count_original_utf8_bytes() {
    for (input, offset) in [("[\"é\",]", 6), ("[\"😀\",]", 8), ("[\"\\u00e9\",]", 10)] {
        Workspace::rejects(
            &format!("x=json.decode({})", serde_json::to_string(input).unwrap()),
            &format!("offset {offset}"),
        );
    }
}

#[test]
fn strict_string_keys_utf8_sorting_struct_provider_fields_and_sequence_arrays() {
    let defs = format!(
        "P=provider(fields=['z','a'])\n{}",
        rule(
            r#"
value={'😀':0,'':1,'z':2,'a':3}
p=P(z=(1,2),a=struct(z=[],a=True))
text=json.encode([value,p,range(3),set([3,1,2]),(4,5)])
"#
        )
    );
    let ws = Workspace::new(&defs, CHECK);
    assert_eq!(content(&ws.run(&["build", ":check"])),["[{\"a\":3,\"z\":2,\"\":1,\"😀\":0},{\"a\":{\"a\":true,\"z\":[]},\"z\":[1,2]},[0,1,2],[3,1,2],[4,5]]".as_bytes().to_vec()]);
    for keys in ["{1:'bad'}", "{'a':1,2:3}", "{None:1}", "{True:1}"] {
        Workspace::rejects(&format!("x=json.encode({keys})"), "key");
    }
}

#[test]
fn actual_main_files_match_own_paths_and_explicit_native_configuration() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/json/file_reference.json")).unwrap();
    let ws = Workspace::fixture(&fixture);
    // The reference source is unchanged; compare engine-specific expectations,
    // never rewrite the genuine k8 root and call that byte parity.
    let output = ws.run(&["build", ":check"]);
    let expected = "[{\"path\":\"input.txt\",\"root\":\"\",\"short_path\":\"input.txt\"},{\"path\":\"bazel-out/k8-fastbuild/bin/file_encoding.json\",\"root\":\"bazel-out/k8-fastbuild/bin\",\"short_path\":\"file_encoding.json\"},[3,1,2],[0,1,2]]";
    assert_eq!(content(&output), [expected.as_bytes().to_vec()]);
    let defs = fixture["files"]["defs.bzl"].as_str().unwrap().replace(
        "    ctx.actions.write(out,json.encode([ctx.file.src,out,set([3,1,2]),range(3)]))",
        r#"    for f in [ctx.file.src,out]:
        encoded=json.decode(json.encode(f))
        if encoded['path']!=f.path or encoded['short_path']!=f.short_path:
            fail('File adapter disagrees with actual File')
        root=encoded['root']
        if f.path!=(root+'/' if root else '')+f.short_path:
            fail('File root relation')
    ctx.actions.write(out,json.encode([ctx.file.src,out,set([3,1,2]),range(3)]))"#,
    );
    ws.file("defs.bzl", defs);
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [expected.as_bytes().to_vec()]
    );
}

#[test]
fn external_file_profiles_are_explicitly_rejected_not_silently_main() {
    let ws = Workspace::new(
        r#"
def _check(ctx):
    print('JSON_EXTERNAL_ENTER')
    json.encode(ctx.file.src)
    fail('GUARD_BYPASSED')
check=rule(implementation=_check,attrs={'src':attr.label(allow_single_file=True)})
"#,
        "load(':defs.bzl','check')\ncheck(name='check',src='@dep//:input.txt')\n",
    );
    ws.file("MODULE.bazel","module(name='json_fixture')\nbazel_dep(name='dep',version='1')\nlocal_path_override(module_name='dep',path='dep')\n");
    ws.file("dep/MODULE.bazel", "module(name='dep',version='1')\n");
    ws.file("dep/BUILD.bazel", "exports_files(['input.txt'])\n");
    ws.file("dep/input.txt", "data");
    let output = ws.run(&["build", ":check"]);
    rejected(&output, "unsupported");
    assert!(errors(&output).contains("JSON_EXTERNAL_ENTER"));
}

#[test]
fn labels_callables_contexts_and_native_info_are_explicitly_unsupported() {
    for value in [
        "Label('//:check')",
        "json",
        "json.encode",
        "len",
        "ctx",
        "ctx.actions",
    ] {
        Workspace::rejects(&format!("x=json.encode({value})"), "unsupported");
    }
    for value in [
        "DefaultInfo()",
        "RunEnvironmentInfo()",
        "platform_common.TemplateVariableInfo({})",
    ] {
        Workspace::rejects(
            &format!("x=json.encode({value})"),
            "cannot encode native Info as JSON",
        );
    }
    let ws = Workspace::new(
        r#"
def _check(ctx):
    json.encode(ctx.attr.g[PackageSpecificationInfo])
    fail('GUARD_BYPASSED')
check=rule(implementation=_check,attrs={'g':attr.label(providers=[PackageSpecificationInfo])})
"#,
        "load(':defs.bzl','check')\npackage_group(name='g',packages=['//...'])\ncheck(name='check',g=':g')\n",
    );
    rejected(
        &ws.run(&["build", ":check"]),
        "cannot encode native Info as JSON",
    );
    ws.untouched();
}

#[test]
fn provider_named_file_and_file_shaped_struct_do_not_gain_file_access() {
    let defs = format!(
        "File=provider(fields=['path','root','short_path','extra'])\n{}",
        rule(
            r#"
p=File(path='nonsense',root='fake',short_path='wrong',extra=7)
s=struct(path='nonsense',root='fake',short_path='wrong',extra=7)
text=json.encode([p,s])
"#
        )
    );
    let ws = Workspace::new(&defs, CHECK);
    let field = r#"{"extra":7,"path":"nonsense","root":"fake","short_path":"wrong"}"#;
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [format!("[{field},{field}]").into_bytes()]
    );
    Workspace::rejects(
        "x=json.encode(struct(path='fake',to_json=json.encode))",
        "unsupported",
    );
}

#[test]
fn indentation_preserves_tokens_duplicates_arbitrary_prefix_and_empty_containers() {
    expect(
        r#"text=json.indent(' {"z":1e+02,"z":2,"a":"\\u0061","empty":[]} \n',prefix='>',indent='..')"#,
        "{\n>..\"z\": 1e+02,\n>..\"z\": 2,\n>..\"a\": \"\\u0061\",\n>..\"empty\": []\n>}",
    );
    expect(
        "text=json.encode_indent([{},[]],prefix='P',indent='x')",
        "[\nPx{},\nPx[]\nP]",
    );
    expect(
        "text=json.encode_indent({'a':[1]})",
        "{\n\t\"a\": [\n\t\t1\n\t]\n}",
    );
}

#[test]
fn indentation_pinned_nonvalidator_and_numeric_eof_quirks() {
    expect(
        "text=json.encode([json.indent('[}'),json.indent('null trailing'),json.indent('1 '),json.indent('{}'),json.indent('[]')])",
        r#"["[}","null","1","{}","[]"]"#,
    );
    for call in ["json.indent('1')", "json.encode_indent(1)"] {
        Workspace::rejects(&format!("x={call}"), "");
    }
}

#[test]
fn native_depth_64_accepts_65_falls_back_but_encoder_rejects() {
    expect(
        r#"
s='['*64+'0'+']'*64
d=json.decode(s)
if json.encode(d)!=s:
    fail('64 containers')
fallback=[]
r=json.decode('['*65+'0'+']'*65,default=fallback)
r.append('same')
text=json.encode(fallback)
"#,
        r#"["same"]"#,
    );
    Workspace::rejects(
        "x=0\nfor i in range(65):\n    x=[x]\ny=json.encode(x)",
        "depth",
    );
    Workspace::rejects("x=json.decode('['*65+'0'+']'*65)", "depth");
}

#[test]
fn cycles_reject_but_repeated_aliases_are_not_global_visited_suppressed() {
    expect(
        "a=[1]\ntext=json.encode([a,a,{'a':a,'b':a}])",
        r#"[[1],[1],{"a":[1],"b":[1]}]"#,
    );
    Workspace::rejects("a=[]\na.append(a)\nx=json.encode(a)", "cycle");
    Workspace::rejects("a={}\na['self']=a\nx=json.encode_indent(a)", "cycle");
}

#[test]
fn resource_dag_huge_range_prefix_and_repeated_alias_output_are_bounded() {
    for body in [
        "a=[0]\nfor i in range(18):\n    a=[a,a]\nx=json.encode(a)",
        "x=json.encode(range(1000000000))",
        "x=json.encode_indent(range(100),prefix='x'*1000000)",
        "a='x'*1000000\nx=json.encode([a]*100)",
    ] {
        Workspace::rejects(body, "resource limit");
    }
}

#[test]
fn decode_resource_node_duplicate_and_bigint_limits_never_return_default() {
    // At most ~1.3 MB input each; never ask the process to manufacture GBs.
    // Discarded duplicate values must still consume cumulative work/nodes.
    for body in [
        "x=json.decode('['+'null,'*249999+'null]',default='FALLBACK_RETURNED')",
        "x=json.decode('{'+ '\"x\":[],'*80000+'\"x\":[]}',default='FALLBACK_RETURNED')",
        "x=json.decode('9'*1000000,default='FALLBACK_RETURNED')",
    ] {
        Workspace::rejects(body, "resource limit");
    }
    expect(
        "a=json.decode('['+'0,'*100001+'0]')\nb=json.decode('{'+ '\"x\":[],'*40000+'\"x\":[7]}')\ntext=json.encode([len(a),b])",
        "[100002,{\"x\":[7]}]",
    );
}

#[test]
fn generated_metadata_candidate_decode_produces_real_consumer_summary() {
    // Generated, deterministic public-metadata-shaped data, not captured network
    // bytes. The JSON is a bzl literal, never a large attribute/schema payload.
    // 120 releases * 80 files * 13 nodes/file + 7/release + root = 125641 nodes.
    // Member storage alone: (120*3 + 9600*6)*1024 = 59351040 weighted bytes;
    // file container openings alone add 19660800, already above the legacy cap.
    let releases = (0..120).map(|release| {
        let files = (0..80).map(|file| {
            let index = release * 80 + file;
            serde_json::json!({
                "filename": format!("sdk-{release}.{file}.linux-amd64.tar.gz"),
                "os": "linux", "arch": "amd64", "kind": "archive",
                "sha256": format!("{index:064x}"), "size": 1_000_000 + index,
            })
        }).collect::<Vec<_>>();
        serde_json::json!({"version": format!("sdk1.{release}"), "stable": release % 2 == 0, "files": files})
    }).collect::<Vec<_>>();
    let input = serde_json::to_string(&releases).unwrap();
    assert!(input.len() < 8 * 1024 * 1024);
    let defs = format!(
        "PAYLOAD={}\n{}",
        serde_json::to_string(&input).unwrap(),
        r#"
def _producer(ctx):
    releases=json.decode(PAYLOAD)
    total=0
    size=0
    stable=0
    for release in releases:
        if sorted(release.keys())!=['files','stable','version']:
            fail('release fields')
        if release['stable']:
            stable+=1
        for f in release['files']:
            if sorted(f.keys())!=['arch','filename','kind','os','sha256','size']:
                fail('file fields')
            if f['os']!='linux' or f['arch']!='amd64' or f['kind']!='archive':
                fail('file values')
            total+=1
            size+=f['size']
    first=releases[0]['files'][0]
    last=releases[-1]['files'][-1]
    summary={'releases':len(releases),'files':total,'size':size,'stable':stable,'first_version':releases[0]['version'],'last_version':releases[-1]['version'],'first':first,'last':last}
    out=ctx.actions.declare_file('metadata-summary.json')
    ctx.actions.write(out,json.encode(summary))
    return [DefaultInfo(files=depset([out]))]
producer=rule(implementation=_producer)
"#,
    );
    let ws = Workspace::new(
        &defs,
        "load(':defs.bzl','producer')\nproducer(name='metadata')\ngenrule(name='check',srcs=[':metadata'],outs=['consumer-summary.json'],cmd='cat $(SRCS) > $@')\n",
    );
    let output = ws.run(&["build", ":check"]);
    counts(&output, 2, 0);
    let bytes = content(&output);
    assert_eq!(bytes.len(), 1);
    let summary: serde_json::Value = serde_json::from_slice(&bytes[0]).unwrap();
    assert_eq!(
        summary,
        serde_json::json!({
            "releases": 120, "files": 9600, "stable": 60,
            "size": 9600u64 * 1_000_000 + 9599u64 * 9600 / 2,
            "first_version": "sdk1.0", "last_version": "sdk1.119",
            "first": releases[0]["files"][0], "last": releases[119]["files"][79],
        })
    );
    eprintln!(
        "generated metadata: input_bytes={} nodes=125641 summary={summary}",
        input.len()
    );
}

#[test]
fn typed_leaf_provider_key_reexport_and_frozen_fields_survive_real_dependency() {
    let ws = Workspace::new(
        r#"
load(':leaf.bzl','Leaf','Frozen')
load(':alias.bzl','Alias')
def _producer(ctx):
    if Alias!=Leaf:
        fail('provider key identity')
    return [Alias(a=json.decode(ctx.attr.data),z=Frozen)]
producer=rule(implementation=_producer,attrs={'data':attr.string()})
def _check(ctx):
    p=ctx.attr.dep[Leaf]
    if p!=ctx.attr.dep[Alias]:
        fail('provider lookup alias')
    out=ctx.actions.declare_file('check.out')
    ctx.actions.write(out,json.encode(p))
    return [DefaultInfo(files=depset([out]))]
check=rule(implementation=_check,attrs={'dep':attr.label(providers=[Leaf])})
"#,
        "load(':defs.bzl','producer','check')\nproducer(name='p',data='[1]')\ncheck(name='check',dep=':p')\n",
    );
    ws.file(
        "leaf.bzl",
        "Leaf=provider(fields=['z','a'])\nFrozen=Leaf(z=json.decode('[0]'),a=True)\n",
    );
    ws.file("alias.bzl", "load(':leaf.bzl','Leaf')\nAlias=Leaf\n");
    assert_eq!(
        content(&ws.run(&["build", ":check"])),
        [br#"{"a":[1],"z":{"a":true,"z":[0]}}"#.to_vec()]
    );
}
