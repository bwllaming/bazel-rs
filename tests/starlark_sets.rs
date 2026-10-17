//! H1 host-evaluator contracts, using the ordinary root dependency graph.
//!
//! Expectations: artifacts/starlark-set-{contract,edge-contract,constructor-contract}.json.
//! Post-error receiver expectations are SOURCE_DERIVED from pinned Java, not
//! executed Java-host observations (starlark-set-post-error-contract.json: zero
//! executions). H2 production globals and provider/File traversal are not tested
//! or enabled here. Generic Unicode repr parity is an existing separate gap.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::hash::Hasher;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use allocative::Allocative;
use bazel_rs::starlark_types::{File, alloc_file, analysis_globals};
use starlark::codemap::FileSpanRef;
use starlark::collections::StarlarkHasher;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::{BeforeStmtFunc, BeforeStmtFuncDyn, Evaluator, FileLoader, ReturnFileLoader};
use starlark::syntax::{AstModule, Dialect, DialectTypes};
use starlark::values::starlark_value;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Trace, Value,
};
use starlark::{environment::LibraryExtension, starlark_module, starlark_simple_value};

fn globals() -> Globals {
    GlobalsBuilder::extended_by(&[LibraryExtension::SetType, LibraryExtension::StructType])
        .with(analysis_globals)
        .build()
}

fn run<'v>(
    module: &Module<'v>,
    source: &str,
    loader: Option<&dyn FileLoader>,
) -> Result<Value<'v>, String> {
    // Extended statements exercise the generic interpreter's module-slot path;
    // they do NOT claim that Bazel .bzl permits top-level augmented rebinding.
    let dialect = Dialect {
        enable_top_level_stmt: true,
        ..Dialect::Standard
    };
    let ast = AstModule::parse("h1-set-host.star", source.to_owned(), &dialect)
        .unwrap_or_else(|e| panic!("invalid host fixture:\n{source}\n{e}"));
    let mut eval = Evaluator::new(module);
    if let Some(loader) = loader {
        eval.set_loader(loader);
    }
    eval.eval_module(ast, &globals()).map_err(|e| e.to_string())
    // Evaluator is dropped here, even on error. The caller retains Module.
}

fn pass(source: &str) {
    Module::with_temp_heap(|module| {
        let result =
            run(&module, source, None).unwrap_or_else(|e| panic!("fixture:\n{source}\n{e}"));
        assert_eq!(result.to_repr(), "True", "fixture:\n{source}");
    });
}

fn reject(source: &str, needles: &[&str]) {
    Module::with_temp_heap(|module| {
        let error = run(&module, source, None).expect_err(source);
        diagnostic(&error, needles, source);
    });
}

fn diagnostic(error: &str, needles: &[&str], source: &str) {
    let lower = error.to_lowercase();
    assert!(
        needles.iter().any(|n| lower.contains(n)),
        "expected one of {needles:?}; fixture:\n{source}\n{error}"
    );
}

fn frozen(source: &str) -> FrozenModule {
    Module::with_temp_heap(|module| {
        run(&module, source, None).unwrap();
        module.freeze().unwrap()
    })
}

fn post_error(source: &str, expected: &str, needles: &[&str]) {
    Module::with_temp_heap(|module| {
        let error = run(&module, source, None).expect_err(source);
        diagnostic(&error, needles, source);
        assert_eq!(module.get("S").unwrap().to_repr(), expected, "{source}");
        assert!(module.get("UNREACHED").is_none());
    });
}

const METHODS: &str = r#"["add", "clear", "difference", "difference_update", "discard", "intersection", "intersection_update", "isdisjoint", "issubset", "issuperset", "pop", "remove", "symmetric_difference", "symmetric_difference_update", "union", "update"]"#;
const OPS: [(&str, &str); 4] = [
    ("|", "[3, 1, 2, 4]"),
    ("&", "[3, 2]"),
    ("-", "[1]"),
    ("^", "[1, 4]"),
];

#[test]
fn constructor_metadata_and_exact_method_surface() {
    pass(&format!(
        r#"
type(set) == "builtin_function_or_method" and repr(set) == "<built-in function set>" and str(set) == "<built-in function set>" and dir(set) == [] and not hasattr(set, "type") and dir(set()) == {METHODS} and type(set()) == "set"
"#
    ));
    reject("set[int]", &["not supported", "unsupported", "index"]);
    reject("set.type", &["attribute"]);
}

#[test]
fn constructor_inputs_and_fresh_ordered_storage() {
    pass(
        r#"
a = [3, 1, 3]
s = set(a)
a.append(9)
t = set(s)
t.add(8)
list(s) == [3, 1] and list(t) == [3, 1, 8] and list(set((3, 1, 3))) == [3, 1] and list(set({"b": 1, "a": 2})) == ["b", "a"] and list(set(range(3))) == [0, 1, 2] and list(set()) == []
"#,
    );
    for input in ["None", "1", "\"abc\"", "depset([1])"] {
        reject(&format!("set({input})"), &["type", "iterable"]);
    }
    for call in ["set([], [])", "set(elements=[])", "set(arg=[])"] {
        reject(call, &["argument", "parameter", "positional"]);
    }
}

#[test]
fn constructor_frozen_aliases_equal_callable_and_unhashable() {
    let one = frozen("R = set\n");
    let two = frozen("R = set\n");
    let modules = HashMap::from([("one", &one), ("two", &two)]);
    let loader = ReturnFileLoader { modules: &modules };
    Module::with_temp_heap(|module| {
        let result = run(
            &module,
            "load('one', R1='R')\nload('two', R2='R')\nR1 == R2 and R1 == set and R2 == set and R1 != list and list(R1([2,1,2])) == [2,1] and type(R2) == 'builtin_function_or_method' and dir(R2) == []\n",
            Some(&loader),
        ).unwrap();
        assert_eq!(result.to_repr(), "True");
    });
    for expression in ["{set: 1}", "set([set])", "set().add(set)"] {
        reject(expression, &["unhashable", "not hashable"]);
    }
    for expression in ["{R: 1}", "set([R])"] {
        Module::with_temp_heap(|module| {
            let error = run(
                &module,
                &format!("load('one', 'R')\n{expression}\n"),
                Some(&loader),
            )
            .unwrap_err();
            diagnostic(&error, &["unhashable", "not hashable"], expression);
        });
    }
}

// These real host values deliberately separate normal unhashability from an
// unrelated hashing failure, and abbreviated Display from full collect_repr.
#[allow(unsafe_code)] // Upstream derives implement marker traits.
mod host_values {
    use super::*;

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
    pub struct ReprMember;
    starlark_simple_value!(ReprMember);
    impl fmt::Display for ReprMember {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("ABBREVIATED")
        }
    }
    #[starlark_value(type = "repr_member")]
    impl<'v> StarlarkValue<'v> for ReprMember {
        fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
            hasher.write_u32(73);
            Ok(())
        }
        fn collect_repr(&self, output: &mut String) {
            output.push_str("full_member(payload=73)");
        }
    }

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
    pub struct BrokenHash;
    starlark_simple_value!(BrokenHash);
    impl fmt::Display for BrokenHash {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("broken_hash")
        }
    }
    #[starlark_value(type = "broken_hash")]
    impl<'v> StarlarkValue<'v> for BrokenHash {
        fn write_hash(&self, _: &mut StarlarkHasher) -> starlark::Result<()> {
            Err(starlark::Error::new_other(anyhow::anyhow!(
                "deliberate unrelated host write_hash failure"
            )))
        }
    }

    #[derive(Debug, Trace, ProvidesStaticType, NoSerialize, Allocative)]
    pub struct Attribute<'v> {
        pub slot: RefCell<Value<'v>>,
        #[trace(unsafe_ignore)]
        #[allocative(skip)]
        pub events: Arc<Mutex<Vec<&'static str>>>,
    }
    impl fmt::Display for Attribute<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("host_attribute")
        }
    }
    #[starlark_value(type = "host_attribute")]
    impl<'v> StarlarkValue<'v> for Attribute<'v> {
        fn get_attr(&self, name: &str, _: Heap<'v>) -> Option<Value<'v>> {
            if name != "slot" {
                return None;
            }
            self.events.lock().unwrap().push("get");
            Some(*self.slot.borrow())
        }
        fn set_attr(&self, name: &str, value: Value<'v>) -> starlark::Result<()> {
            assert_eq!(name, "slot");
            self.events.lock().unwrap().push("set");
            *self.slot.borrow_mut() = value;
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    pub struct IterCounts {
        pub starts: usize,
        pub stops: usize,
        pub active: usize,
    }

    // A host iterator protocol witness, not a replacement/mock set. Container
    // mutation guards are independently tested on genuine Set/Dict/List below.
    #[derive(Debug, Trace, ProvidesStaticType, NoSerialize, Allocative)]
    pub struct CountedIterable {
        pub len: usize,
        #[trace(unsafe_ignore)]
        #[allocative(skip)]
        pub counts: Arc<Mutex<IterCounts>>,
    }
    impl fmt::Display for CountedIterable {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("counted_host_iterable")
        }
    }
    #[starlark_value(type = "counted_host_iterable")]
    impl<'v> StarlarkValue<'v> for CountedIterable {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            (name == "active").then(|| heap.alloc(self.counts.lock().unwrap().active as i32))
        }
        unsafe fn iterate(&self, me: Value<'v>, _: Heap<'v>) -> starlark::Result<Value<'v>> {
            let mut counts = self.counts.lock().unwrap();
            counts.starts += 1;
            counts.active += 1;
            Ok(me)
        }
        unsafe fn iter_next(&self, index: usize, heap: Heap<'v>) -> Option<Value<'v>> {
            assert!(
                self.counts.lock().unwrap().active > 0,
                "iterator used after release"
            );
            (index < self.len).then(|| heap.alloc(index as i32))
        }
        unsafe fn iter_stop(&self) {
            let mut counts = self.counts.lock().unwrap();
            assert!(counts.active > 0, "iterator released more than once");
            counts.active -= 1;
            counts.stops += 1;
        }
    }
}

#[test]
fn repr_uses_member_collect_repr_and_unicode_remains_data() {
    pass("repr(set()) == 'set()' and repr(set([3,1,3])) == 'set([3, 1])'");
    // Deliberately not a claim about generic Unicode escaping in repr().
    pass("list(set(['λ', '雪', 'λ'])) == ['λ', '雪'] and '雪' in set(['雪'])");
    Module::with_temp_heap(|module| {
        module.set("member", module.heap().alloc(host_values::ReprMember));
        let value = run(&module, "S = set([member])\nS\n", None).unwrap();
        assert_eq!(value.to_repr(), "set([full_member(payload=73)])");
        assert_eq!(value.to_string(), "set([full_member(payload=73)])");
        let owner = module.freeze().unwrap();
        assert_eq!(
            owner.get("S").unwrap().value().to_repr(),
            "set([full_member(payload=73)])"
        );
    });
}

#[test]
fn membership_unhashability_is_false_but_host_errors_propagate() {
    pass(
        "s = set([1])\n[] not in s and {} not in s and set() not in s and ([],) not in s and set not in s and 1 in s and 9 not in s\n",
    );
    for expression in ["bad in set()", "bad in set([1])", "(bad,) in set([1])"] {
        Module::with_temp_heap(|module| {
            module.set("bad", module.heap().alloc(host_values::BrokenHash));
            let error = run(&module, expression, None).expect_err(expression);
            assert!(
                error.contains("deliberate unrelated host write_hash failure"),
                "{error}"
            );
        });
    }
}

#[test]
fn mutable_and_frozen_sets_are_unhashable() {
    for value in ["set()", "set([1])", "[1]", "{}", "([],)"] {
        for expression in [format!("set([{value}])"), format!("{{{value}: 1}}")] {
            reject(&expression, &["unhashable", "not hashable"]);
        }
    }
    let owner = frozen("S = set([1])\nE = set()\n");
    let modules = HashMap::from([("owner", &owner)]);
    let loader = ReturnFileLoader { modules: &modules };
    for expression in ["{S: 1}", "{E: 1}", "set([S])", "set([E])"] {
        Module::with_temp_heap(|module| {
            let error = run(
                &module,
                &format!("load('owner', 'S', 'E')\n{expression}\n"),
                Some(&loader),
            )
            .unwrap_err();
            diagnostic(&error, &["unhashable", "not hashable"], expression);
        });
    }
}

#[test]
fn equality_truthiness_and_boolean_integer_distinctions() {
    pass(
        "set([1,2]) == set([2,1]) and set([1]) != [1] and set([1]) != (1,) and not set() and bool(set([0])) and len(set([1,1,2])) == 2 and list(set([False,0,True,1])) == [False,0,True,1] and len(set([False,0,True,1])) == 4",
    );
}

#[test]
fn primitive_mutators_return_none_and_pop_first() {
    pass(
        r#"
def probe():
    s = set([3,1,2])
    first = s.pop()
    ok = first == 3 and list(s) == [1,2]
    ok = ok and s.add(3) == None and s.add(1) == None and list(s) == [1,2,3]
    ok = ok and s.discard(99) == None and s.remove(2) == None and list(s) == [1,3]
    return ok and s.clear() == None and list(s) == [] and s.clear() == None
probe()
"#,
    );
    reject("set().pop()", &["empty"]);
    reject("set([1]).remove(2)", &["not", "missing"]);
    for method in ["add", "discard", "remove"] {
        reject(
            &format!("set().{method}([])"),
            &["unhashable", "not hashable"],
        );
    }
}

#[test]
fn ordered_binary_algebra_is_pure_and_fresh() {
    for (op, expected) in OPS {
        Module::with_temp_heap(|module| {
            let source = format!(
                "L = set([3,1,2])\nR = set([2,4,3])\nOUT = L {op} R\nlist(OUT) == {expected} and list(L) == [3,1,2] and list(R) == [2,4,3]\n"
            );
            assert_eq!(
                run(&module, &source, None).unwrap().to_repr(),
                "True",
                "{source}"
            );
            assert_ne!(
                module.get("OUT").unwrap().identity(),
                module.get("L").unwrap().identity()
            );
            assert_ne!(
                module.get("OUT").unwrap().identity(),
                module.get("R").unwrap().identity()
            );
            assert_eq!(
                run(&module, "OUT.add(99)\n99 not in L and 99 not in R\n", None)
                    .unwrap()
                    .to_repr(),
                "True"
            );
        });
        for rhs in ["[]", "()", "{}", "range(0)", "None"] {
            reject(
                &format!("set() {op} {rhs}"),
                &["type", "unsupported", "not supported"],
            );
            reject(
                &format!("{rhs} {op} set()"),
                &["type", "unsupported", "not supported"],
            );
        }
    }
}

#[test]
fn collection_algebra_variadic_zero_arguments_and_left_order() {
    pass(
        r#"
s = set([3,1,2])
list(s.union()) == [3,1,2] and list(s.intersection()) == [3,1,2] and list(s.difference()) == [3,1,2] and list(s.union([2,4],[5,4])) == [3,1,2,4,5] and list(s.intersection([2,3],[3,2])) == [3,2] and list(s.difference([1],[2])) == [3] and list(s.symmetric_difference([2,4,4])) == [3,1,4] and list(s) == [3,1,2]
"#,
    );
    for method in ["union", "intersection", "difference"] {
        Module::with_temp_heap(|module| {
            run(&module, &format!("S=set([1])\nOUT=S.{method}()\n"), None).unwrap();
            assert_ne!(
                module.get("S").unwrap().identity(),
                module.get("OUT").unwrap().identity()
            );
        });
    }
    pass(
        r#"
s = set([3,1,2])
list(s.union({4:0,2:0},range(2))) == [3,1,2,4,0] and list(s.intersection({2:0,3:0})) == [3,2] and list(s.difference(range(2))) == [3,2] and list(s.symmetric_difference({2:0,4:0})) == [3,1,4]
"#,
    );
}

#[test]
fn update_family_variadic_collections_multiplicities_and_self_aliases() {
    pass(
        r#"
def probe():
    s=set([3,1,2]); alias=s
    ok=s.update() == None and s.intersection_update() == None and s.difference_update() == None
    ok=ok and s.update([4],[5]) == None and list(alias) == [3,1,2,4,5]
    ok=ok and s.intersection_update([1,2,4],[2,4]) == None and list(alias) == [2,4]
    ok=ok and s.difference_update([2],[]) == None and list(alias) == [4]
    ok=ok and s.symmetric_difference_update([4,6,6]) == None and list(alias) == [6]
    s.update({7:0,6:0},range(2))
    ok=ok and list(alias) == [6,7,0,1]
    s.intersection_update({1:0,7:0,6:0})
    s.difference_update(range(1,2))
    ok=ok and list(alias) == [6,7]
    s.update(alias); s.intersection_update(alias)
    ok=ok and list(alias) == [6,7]
    s.difference_update(alias)
    return ok and list(s) == [] and list(alias) == []
probe()
"#,
    );
}

#[test]
fn predicates_validate_whole_operands_before_shortcuts() {
    pass(
        "s=set([3,1,2])\ns.isdisjoint([9,9]) and s.issubset([1,2,3,3]) and s.issuperset([1,1]) and s.issubset({1:0,2:0,3:0}) and s.issuperset(range(1,3)) and not s.isdisjoint(range(3)) and not s.issubset([1]) and not s.issuperset([9])\n",
    );
    for (receiver, method, operand) in [
        ("set([1])", "isdisjoint", "[1, []]"),
        ("set([1])", "issuperset", "[2, []]"),
        ("set()", "issubset", "[1, []]"),
        ("set()", "isdisjoint", "[1, []]"),
        ("set()", "difference", "[[]]"),
        ("set()", "intersection", "[[]]"),
        ("set([1])", "intersection", "[], [[]]"),
        ("set([1])", "difference", "[1], [[]]"),
    ] {
        reject(
            &format!("{receiver}.{method}({operand})"),
            &["unhashable", "not hashable"],
        );
    }
}

#[test]
fn method_positional_signatures_and_collection_rejections() {
    for method in [
        "add",
        "discard",
        "remove",
        "isdisjoint",
        "issubset",
        "issuperset",
        "symmetric_difference",
        "symmetric_difference_update",
    ] {
        for arguments in ["", "[], []", "element=1", "other=[]"] {
            reject(
                &format!("set().{method}({arguments})"),
                &["argument", "parameter", "positional"],
            );
        }
    }
    for method in ["clear", "pop"] {
        reject(
            &format!("set().{method}(1)"),
            &["argument", "parameter", "positional"],
        );
    }
    for method in [
        "union",
        "intersection",
        "difference",
        "update",
        "intersection_update",
        "difference_update",
    ] {
        reject(
            &format!("set().{method}(other=[])"),
            &["argument", "parameter", "named", "keyword"],
        );
    }
    for method in [
        "union",
        "intersection",
        "difference",
        "symmetric_difference",
        "update",
        "intersection_update",
        "difference_update",
        "symmetric_difference_update",
        "isdisjoint",
        "issubset",
        "issuperset",
    ] {
        for operand in ["'ab'", "None", "1", "depset([1])"] {
            reject(
                &format!("set([1]).{method}({operand})"),
                &["type", "iterable", "collection"],
            );
        }
    }
}

#[test]
fn source_derived_per_operand_commit_post_error_state() {
    // SOURCE_DERIVED Java contract; native state really inspected after failure.
    for (call, expected) in [
        (
            "S.update(['first'], ['good', []])",
            "set([\"seed\", \"first\"])",
        ),
        ("S.update(['good', []])", "set([\"seed\"])"),
    ] {
        post_error(
            &format!("S=set(['seed'])\n{call}\nUNREACHED=True\n"),
            expected,
            &["unhashable", "not hashable"],
        );
    }
    for (call, expected) in [
        ("S.intersection_update([3,2], [2, []])", "set([3, 2])"),
        ("S.difference_update([1], [2, []])", "set([3, 2])"),
        ("S.intersection_update([2, []])", "set([3, 1, 2])"),
        ("S.difference_update([2, []])", "set([3, 1, 2])"),
        ("S.symmetric_difference_update([2, []])", "set([3, 1, 2])"),
    ] {
        post_error(
            &format!("S=set([3,1,2])\n{call}\nUNREACHED=True\n"),
            expected,
            &["unhashable", "not hashable"],
        );
    }
}

const MUTATIONS: [&str; 9] = [
    "s.add(1)",
    "s.clear()",
    "s.discard(99)",
    "s.remove(1)",
    "s.pop()",
    "s.update()",
    "s.intersection_update()",
    "s.difference_update()",
    "s.symmetric_difference_update([])",
];

#[test]
fn frozen_receiver_guards_cover_noops_and_precede_bad_content() {
    for elements in ["[]", "[1]"] {
        let owner = frozen(&format!("S=set({elements})\n"));
        let modules = HashMap::from([("owner", &owner)]);
        let loader = ReturnFileLoader { modules: &modules };
        for mutation in MUTATIONS.into_iter().chain([
            "s.add([])",
            "s.discard([])",
            "s.remove([])",
            "s.update(1)",
            "s.intersection_update([[]])",
            "s.difference_update([[]])",
            "s.symmetric_difference_update([[]])",
        ]) {
            Module::with_temp_heap(|module| {
                let source =
                    format!("load('owner', 'S')\ndef f():\n    s=S\n    {mutation}\nf()\n");
                let error = run(&module, &source, Some(&loader)).expect_err(&source);
                diagnostic(&error, &["frozen", "immutable"], &source);
                assert!(!error.to_lowercase().contains("unhashable"), "{error}");
            });
        }
        for call in ["S.add()", "S.update(unknownkw=[])"] {
            Module::with_temp_heap(|module| {
                let source = format!("load('owner', 'S')\n{call}\n");
                let error = run(&module, &source, Some(&loader)).unwrap_err();
                diagnostic(&error, &["argument", "parameter", "keyword"], &source);
                assert!(!error.to_lowercase().contains("frozen"), "{error}");
            });
        }
    }
}

#[test]
fn active_iteration_guards_all_mutators_and_augmented_noops() {
    let augmented = ["s |= set()", "s &= set([1])", "s -= set()", "s ^= set()"];
    for mutation in MUTATIONS
        .into_iter()
        .chain(augmented)
        .chain(["s.add([])", "s.update([[]])"])
    {
        let source = format!(
            "S=set([1])\ndef f():\n    s=S\n    for x in s:\n        {mutation}\nf()\nUNREACHED=True\n"
        );
        post_error(&source, "set([1])", &["iteration", "iterat", "immutable"]);
    }
}

#[test]
fn iterator_cleanup_after_break_and_evaluation_error() {
    pass(
        "def f():\n    s=set([1,2])\n    for x in s:\n        break\n    s.add(3)\n    return list(s) == [1,2,3]\nf()\n",
    );
    Module::with_temp_heap(|module| {
        let error=run(&module,"S=set([1,2])\ndef f():\n    for x in S:\n        fail('intentional iterator exit')\nf()\n",None).unwrap_err();
        assert!(error.contains("intentional iterator exit"));
        // A second evaluator genuinely executes, unlike text after failed eval.
        assert_eq!(
            run(&module, "S.add(3)\nlist(S) == [1,2,3]\n", None)
                .unwrap()
                .to_repr(),
            "True"
        );
    });
}

#[test]
fn augmented_local_captured_and_module_slots_preserve_identity() {
    for (op, expected) in OPS {
        pass(&format!(
            r#"
def f():
    s=set([3,1,2]); alias=s
    def observe():
        return list(s)
    s {op}= set([2,4,3])
    return list(alias) == {expected} and observe() == {expected}
f()
"#
        ));
        Module::with_temp_heap(|module| {
            // Generic engine module targets, NOT a Bazel .bzl success claim.
            let source = format!(
                "S=set([3,1,2])\nALIAS=S\nS {op}= set([2,4,3])\nlist(S) == {expected} and list(ALIAS) == {expected}\n"
            );
            assert_eq!(
                run(&module, &source, None).unwrap().to_repr(),
                "True",
                "{source}"
            );
            assert_eq!(
                module.get("S").unwrap().identity(),
                module.get("ALIAS").unwrap().identity()
            );
        });
    }
}

#[test]
fn indexed_augmented_evaluation_order_and_replaced_rhs_slot() {
    for (op, expected) in OPS {
        pass(&format!(
            r#"
def f():
    events=[]
    holder=[set([3,1,2])]
    original=holder[0]
    replacement=set([99])
    def owner():
        events.append("owner")
        return holder
    def index():
        events.append("index")
        return 0
    def rhs():
        events.append("rhs")
        holder[0]=replacement
        return set([2,4,3])
    owner()[index()] {op}= rhs()
    return events == ["owner","index","rhs"] and list(original) == {expected} and list(holder[0]) == {expected} and list(replacement) == [99]
f()
"#
        ));
    }
}

#[test]
fn failed_rhs_or_operation_does_not_store() {
    for (op, _) in OPS {
        for rhs in ["fail('rhs abort')", "[]"] {
            Module::with_temp_heap(|module| {
                let source = format!(
                    r#"
S=set([1])
REPLACEMENT=set([99])
HOLDER=[S]
def rhs():
    HOLDER[0]=REPLACEMENT
    return {rhs}
def f():
    HOLDER[0] {op}= rhs()
f()
UNREACHED=True
"#
                );
                let error = run(&module, &source, None).unwrap_err();
                diagnostic(
                    &error,
                    if rhs.starts_with("fail") {
                        &["rhs abort"]
                    } else {
                        &["type", "unsupported", "not supported"]
                    },
                    &source,
                );
                assert_eq!(module.get("S").unwrap().to_repr(), "set([1])");
                assert_eq!(module.get("HOLDER").unwrap().to_repr(), "[set([99])]");
                assert!(module.get("UNREACHED").is_none());
            });
        }
    }
}

#[test]
fn immutable_tuple_and_struct_writeback_fail_after_mutation() {
    // SOURCE_DERIVED post-error state. The reference probe observed the tuple
    // writeback error, not a subsequently executed print of its receiver.
    for (op, expected) in OPS {
        for (owner, target) in [("(S,)", "HOLDER[0]"), ("struct(slot=S)", "HOLDER.slot")] {
            post_error(
                &format!(
                    "S=set([3,1,2])\nHOLDER={owner}\ndef f():\n    {target} {op}= set([2,4,3])\nf()\nUNREACHED=True\n"
                ),
                &format!("set({expected})"),
                &[
                    "immutable",
                    "unsupported",
                    "not supported",
                    "attribute",
                    "assign",
                ],
            );
        }
    }
}

#[test]
fn real_host_attribute_getter_setter_order_and_failed_op() {
    for (op, expected) in OPS {
        for valid in [true, false] {
            Module::with_temp_heap(|module| {
                run(&module, "S=set([3,1,2])\n", None).unwrap();
                let events = Arc::new(Mutex::new(Vec::new()));
                module.set(
                    "HOST",
                    module
                        .heap()
                        .alloc_complex_no_freeze(host_values::Attribute {
                            slot: RefCell::new(module.get("S").unwrap()),
                            events: events.clone(),
                        }),
                );
                // RHS reads slot as well: native event order must be get(LHS),
                // get(RHS), set(writeback), and no setter on operator failure.
                let rhs = if valid { "set([2,4,3])" } else { "[]" };
                let source = format!(
                    "def rhs():\n    observed=HOST.slot\n    return {rhs}\ndef f():\n    HOST.slot {op}= rhs()\nf()\n"
                );
                let result = run(&module, &source, None);
                if valid {
                    result.unwrap();
                    assert_eq!(*events.lock().unwrap(), ["get", "get", "set"]);
                    assert_eq!(
                        module.get("S").unwrap().to_repr(),
                        format!("set({expected})")
                    );
                    let stored = module
                        .get("HOST")
                        .unwrap()
                        .get_attr("slot", module.heap())
                        .unwrap()
                        .unwrap();
                    assert_eq!(stored.identity(), module.get("S").unwrap().identity());
                } else {
                    let error = result.unwrap_err();
                    diagnostic(&error, &["type", "unsupported", "not supported"], &source);
                    assert_eq!(*events.lock().unwrap(), ["get", "get"]);
                    assert_eq!(module.get("S").unwrap().to_repr(), "set([3, 1, 2])");
                }
            });
        }
    }
}

#[test]
fn frozen_augmented_valid_set_rejects_but_bad_rhs_is_operand_error() {
    let owner = frozen("S=set([1])\n");
    let modules = HashMap::from([("owner", &owner)]);
    let loader = ReturnFileLoader { modules: &modules };
    for (op, _) in OPS {
        for rhs in ["set()", "[]"] {
            Module::with_temp_heap(|module| {
                let source =
                    format!("load('owner','S')\ndef f():\n    s=S\n    s {op}= {rhs}\nf()\n");
                let error = run(&module, &source, Some(&loader)).unwrap_err();
                if rhs == "set()" {
                    diagnostic(&error, &["frozen", "immutable"], &source);
                } else {
                    diagnostic(&error, &["type", "unsupported", "not supported"], &source);
                    assert!(!error.to_lowercase().contains("frozen"), "{error}");
                }
                assert_eq!(owner.get("S").unwrap().value().to_repr(), "set([1])");
            });
        }
    }
}

#[test]
fn self_alias_xor_known_reference_defect_intentional_divergence() {
    // KNOWN_REFERENCE_DEFECT — INTENTIONAL_DIVERGENCE:
    // Bazel 9.3's two-element ^= and symmetric_difference_update self-alias
    // actually crashed with exit 37. Safe empty-original semantics are required
    // here, NOT reference parity or an expectation that native code must crash.
    for elements in ["[]", "[1]", "[1,2]", "[3,1,2]"] {
        for mutation in [
            "s ^= other_alias",
            "s.symmetric_difference_update(other_alias)",
        ] {
            pass(&format!(
                "def f():\n    s=set({elements})\n    other_alias=s\n    {mutation}\n    return list(s) == [] and list(other_alias) == []\nf()\n"
            ));
        }
        pass(&format!(
            "s=set({elements})\na=s\nresult=s ^ a\nlist(result) == [] and list(s) == {elements} and list(a) == {elements}\n"
        ));
    }
    for (op, expected) in [("|", "[1,2]"), ("&", "[1,2]"), ("-", "[]")] {
        pass(&format!(
            "def f():\n    s=set([1,2]); alias=s\n    s {op}= alias\n    return list(s) == {expected} and list(alias) == {expected}\nf()\n"
        ));
    }
}

#[test]
fn nonset_augmented_arithmetic_dict_and_list_regressions() {
    pass(
        r#"
def f():
    n=15; n-=3; n&=10; n^=3
    d={"a":1}; da=d; d|={"b":2,"a":3}
    l=[1]; la=l; l += [2,3]
    return n == 11 and d == {"a":3,"b":2} and da == d and la == [1,2,3] and l == la
f()
"#,
    );
    Module::with_temp_heap(|module| {
        run(
            &module,
            "D={'a':1}\nDA=D\nD |= {'b':2}\nL=[1]\nLA=L\nL += [2]\n",
            None,
        )
        .unwrap();
        assert_eq!(
            module.get("D").unwrap().identity(),
            module.get("DA").unwrap().identity()
        );
        assert_eq!(
            module.get("L").unwrap().identity(),
            module.get("LA").unwrap().identity()
        );
    });
}

#[test]
fn cross_module_factory_results_mutate_independently_of_frozen_owners() {
    let owner = frozen(
        "S=set([3,1])\ndef make():\n    return set(S)\ndef united():\n    return S | set([2])\n",
    );
    let modules = HashMap::from([("owner", &owner)]);
    let loader = ReturnFileLoader { modules: &modules };
    let caller = Module::with_temp_heap(|module| {
        let result=run(&module,"load('owner','S','make','united')\nA=make()\nB=make()\nC=united()\nA.add(9)\nB.remove(3)\nC.add(8)\nlist(S) == [3,1] and list(A) == [3,1,9] and list(B) == [1] and list(C) == [3,1,2,8]\n",Some(&loader)).unwrap();
        assert_eq!(result.to_repr(), "True");
        module.freeze().unwrap()
    });
    let modules = HashMap::from([("caller", &caller)]);
    let loader = ReturnFileLoader { modules: &modules };
    Module::with_temp_heap(|module| {
        assert_eq!(run(&module,"load('caller','A','B','C')\nD=set(A)\nD.add(10)\nlist(A) == [3,1,9] and list(B) == [1] and list(C) == [3,1,2,8] and list(D) == [3,1,9,10]\n",Some(&loader)).unwrap().to_repr(),"True");
    });
    assert_eq!(owner.get("S").unwrap().value().to_repr(), "set([3, 1])");
}

#[test]
fn self_algebra_methods_and_binary_results_are_fresh_without_mutation() {
    for elements in ["[]", "[1]", "[3,1,2]"] {
        for (expression, expected) in [
            ("S.union(ALIAS)", elements),
            ("S.intersection(ALIAS)", elements),
            ("S.difference(ALIAS)", "[]"),
            ("S.symmetric_difference(ALIAS)", "[]"),
            ("S | ALIAS", elements),
            ("S & ALIAS", elements),
            ("S - ALIAS", "[]"),
            ("S ^ ALIAS", "[]"),
        ] {
            Module::with_temp_heap(|module| {
                let source = format!(
                    "S=set({elements})\nALIAS=S\nOUT={expression}\nlist(OUT) == {expected} and list(S) == {elements}\n"
                );
                assert_eq!(
                    run(&module, &source, None).unwrap().to_repr(),
                    "True",
                    "{source}"
                );
                assert_ne!(
                    module.get("OUT").unwrap().identity(),
                    module.get("S").unwrap().identity()
                );
            });
        }
    }
}

#[test]
fn rooted_sets_survive_real_gc_and_freeze_with_native_file_label_members() {
    let owner = Module::with_temp_heap(|module| {
        module.set(
            "FILE",
            alloc_file(
                module.heap(),
                File {
                    is_directory: false,
                    path: "pkg/input.txt".to_owned(),
                    short_path: "pkg/input.txt".to_owned(),
                    owner: "//pkg:rule".to_owned(),
                    is_source: true,
                },
            ),
        );
        module.set("member", module.heap().alloc(host_values::ReprMember));
        run(
            &module,
            "S=set([FILE,Label('//pkg:rule'),member,(1,2)])\nALIAS=S\n",
            None,
        )
        .unwrap();
        // Create unreachable heap pressure, retaining no Rust-side Values.
        // Let normal evaluator safepoints collect; manual GC on a newly created
        // evaluator has no initialized bytecode frame and is not a valid fixture.
        for i in 0..1024 {
            module.heap().alloc(format!("{i}:{}", "x".repeat(1024)));
        }
        let before_collection = module.heap().allocated_bytes();
        run(
            &module,
            "def pressure():\n    for i in range(1000):\n        garbage=[i] * 100\npressure()\n",
            None,
        )
        .unwrap();
        assert!(
            module.heap().allocated_bytes() < before_collection,
            "GC must actually reclaim unreachable allocations before checking rooted members"
        );
        assert_eq!(run(&module,"S.add('after_gc')\nlen(ALIAS) == 5 and FILE in S and Label('//pkg:rule') in S and member in S and list(S)[0].path == 'pkg/input.txt'\n",None).unwrap().to_repr(),"True");
        module.freeze().unwrap()
    });
    let modules = HashMap::from([("owner", &owner)]);
    let loader = ReturnFileLoader { modules: &modules };
    Module::with_temp_heap(|module| {
        assert_eq!(run(&module,"load('owner','S','FILE','member')\nCOPY=set(S)\nCOPY.add('caller')\nlen(S) == 5 and len(COPY) == 6 and FILE in S and member in S and Label('//pkg:rule') in S and list(S)[0].is_source\n",Some(&loader)).unwrap().to_repr(),"True");
    });
}

// Iterator cleanup is an engine-host regression surface, not an executed Java
// post-error comparison. The source-derived Java expectations above remain
// separately labeled; the optional Java host attempt executed ZERO cases.
// These fixtures intentionally use engine-only top-level statements and return
// annotations. None claims that these are accepted in Bazel .bzl files.
fn iterator_eval<'v>(eval: &mut Evaluator<'v, '_, '_>, source: &str) -> Result<Value<'v>, String> {
    let dialect = Dialect {
        enable_top_level_stmt: true,
        enable_types: DialectTypes::Enable,
        ..Dialect::Standard
    };
    let ast = AstModule::parse("iterator-host.star", source.to_owned(), &dialect)
        .unwrap_or_else(|e| panic!("invalid iterator fixture:\n{source}\n{e}"));
    let globals = GlobalsBuilder::extended_by(&[LibraryExtension::SetType])
        .with(iterator_callbacks)
        .build();
    eval.eval_module(ast, &globals).map_err(|e| e.to_string())
}

#[starlark_module]
fn iterator_callbacks(builder: &mut GlobalsBuilder) {
    fn catch_inner<'v>(
        inner: Value<'v>,
        mutation: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<bool> {
        let error = eval
            .eval_function(inner, &[], &[])
            .expect_err("inner loop must fail");
        anyhow::ensure!(
            error.to_string().contains("inner callback abort"),
            "{error}"
        );
        // The caller's outer VM iterator MUST survive this caught nested error.
        let error = eval
            .eval_function(mutation, &[], &[])
            .expect_err("inner unwind incorrectly unlocked its outer iterator");
        let text = error.to_string().to_lowercase();
        anyhow::ensure!(
            text.contains("iterat") || text.contains("immutable"),
            "{error}"
        );
        Ok(true)
    }
}

// Each tuple: constructor, same-evaluator mutation, fresh-evaluator mutation.
const ITER_CONTAINERS: [(&str, &str, &str); 3] = [
    ("set([1,2])", "S.add(9)", "S.add(10)"),
    ("{1:1,2:2}", "S[9]=9", "S[10]=10"),
    ("[1,2]", "S.append(9)", "S.append(10)"),
];

fn assert_recovered_on_same_evaluator(eval: &mut Evaluator<'_, '_, '_>, mutation: &str) {
    let source = format!("{mutation}\nlist(S) == [1,2,9]\n");
    assert_eq!(
        iterator_eval(eval, &source).unwrap().to_repr(),
        "True",
        "{source}"
    );
}

fn assert_recovered_on_fresh_evaluator(module: &Module<'_>, mutation: &str) {
    let source = format!("{mutation}\nlist(S) == [1,2,9,10]\n");
    assert_eq!(
        run(module, &source, None).unwrap().to_repr(),
        "True",
        "{source}"
    );
}

#[test]
fn container_iteration_errors_release_only_acquired_loops_on_same_and_fresh_evaluators() {
    for (constructor, mutation, fresh_mutation) in ITER_CONTAINERS {
        for (body, expected_error) in [
            ("for x in S:\n        fail('body abort')", "body abort"),
            (
                "for x in S:\n        for y in S:\n            fail('nested abort')",
                "nested abort",
            ),
            (
                "for x in S:\n        return fail('return expression abort')",
                "return expression abort",
            ),
            (
                "for x in S:\n        pass\n    fail('after exhaustion abort')",
                "after exhaustion abort",
            ),
            (
                "return [fail('comprehension abort') for x in S]",
                "comprehension abort",
            ),
            (
                "for x in S:\n        for y in None:\n            pass",
                "iter",
            ),
        ] {
            Module::with_temp_heap(|module| {
                let mut eval = Evaluator::new(&module);
                eval.disable_gc();
                let source =
                    format!("S={constructor}\ndef f():\n    {body}\nf()\nUNREACHED=True\n");
                let error = iterator_eval(&mut eval, &source).expect_err(&source);
                diagnostic(&error, &[expected_error], &source);
                assert!(module.get("UNREACHED").is_none());
                assert_recovered_on_same_evaluator(&mut eval, mutation);
                drop(eval);
                assert_recovered_on_fresh_evaluator(&module, fresh_mutation);
            });
        }
    }
}

#[test]
fn return_type_failure_releases_nested_container_loops() {
    for (constructor, mutation, fresh_mutation) in ITER_CONTAINERS {
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.disable_gc();
            let source = format!(
                "S={constructor}\ndef typed() -> int:\n    for x in S:\n        for y in S:\n            return 'wrong return type'\n    return 0\ntyped()\n"
            );
            let error = iterator_eval(&mut eval, &source).unwrap_err();
            diagnostic(&error, &["return type", "return value"], &source);
            assert_recovered_on_same_evaluator(&mut eval, mutation);
            drop(eval);
            assert_recovered_on_fresh_evaluator(&module, fresh_mutation);
        });
    }
}

#[test]
fn container_empty_exhaustion_continue_break_and_nested_return_allow_next_mutation() {
    for (constructor, mutation, fresh_mutation) in ITER_CONTAINERS {
        for body in [
            "for x in S:\n        pass",
            "for x in S:\n        continue",
            "for x in S:\n        break",
            "for x in S:\n        for y in S:\n            return None",
            "for x in S:\n        for y in S:\n            break\n        continue",
        ] {
            Module::with_temp_heap(|module| {
                let mut eval = Evaluator::new(&module);
                eval.disable_gc();
                iterator_eval(
                    &mut eval,
                    &format!("S={constructor}\ndef f():\n    {body}\nf()\n"),
                )
                .unwrap();
                assert_recovered_on_same_evaluator(&mut eval, mutation);
                drop(eval);
                assert_recovered_on_fresh_evaluator(&module, fresh_mutation);
            });
        }
    }
    for (constructor, mutation) in [
        ("set()", "S.add(9)"),
        ("{}", "S[9]=9"),
        ("[]", "S.append(9)"),
    ] {
        pass(&format!(
            "S={constructor}\ndef f():\n    for x in S:\n        fail('empty loop executed')\nf()\n{mutation}\nlist(S) == [9]\n"
        ));
    }
}

#[test]
fn host_iterator_protocol_stops_exactly_once_for_all_exit_paths() {
    for (len, body, starts, fails) in [
        (0, "for x in I:\n        pass", 1, false),
        (2, "for x in I:\n        pass", 1, false),
        (2, "for x in I:\n        continue", 1, false),
        (2, "for x in I:\n        break", 1, false),
        (
            2,
            "for x in I:\n        for y in I:\n            return I.active",
            2,
            false,
        ),
        (
            2,
            "for x in I:\n        return fail('return abort')",
            1,
            true,
        ),
        (
            2,
            "for x in I:\n        return 'return type abort'",
            1,
            true,
        ),
        (
            2,
            "for x in I:\n        for y in I:\n            fail('nested abort')",
            2,
            true,
        ),
        (
            2,
            "for x in I:\n        pass\n    fail('after exhaustion abort')",
            1,
            true,
        ),
        (
            2,
            "return [fail('comprehension abort') for x in I]",
            1,
            true,
        ),
        (
            2,
            "for x in I:\n        for y in None:\n            pass",
            1,
            true,
        ),
    ] {
        Module::with_temp_heap(|module| {
            let counts = Arc::new(Mutex::new(host_values::IterCounts::default()));
            module.set(
                "I",
                module
                    .heap()
                    .alloc_complex_no_freeze(host_values::CountedIterable {
                        len,
                        counts: counts.clone(),
                    }),
            );
            let mut eval = Evaluator::new(&module);
            eval.disable_gc();
            let annotation = if body.contains("return type abort") {
                " -> int"
            } else {
                ""
            };
            let source = format!("def f(){annotation}:\n    {body}\nf()\n");
            let result = iterator_eval(&mut eval, &source);
            if fails {
                result.expect_err(&source);
            } else {
                let value = result.unwrap();
                if body.contains("return I.active") {
                    // Existing ReturnStops semantics release before evaluating
                    // the return expression, not merely before returning it.
                    assert_eq!(value.to_repr(), "0");
                }
            }
            {
                let counts = counts.lock().unwrap();
                assert_eq!(
                    (counts.starts, counts.stops, counts.active),
                    (starts, starts, 0),
                    "{source}"
                );
            }
            iterator_eval(&mut eval, "list(I)\n").unwrap();
            let counts = counts.lock().unwrap();
            assert_eq!(
                (counts.starts, counts.stops, counts.active),
                (starts + 1, starts + 1, 0)
            );
        });
    }
}

#[test]
fn caught_reentrant_vm_error_keeps_outer_container_iteration_locked() {
    for (constructor, mutation, fresh_mutation) in ITER_CONTAINERS {
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.disable_gc();
            let source = format!(
                r#"
S={constructor}
CAUGHT=[]
def mutation():
    {mutation}
def inner():
    for x in S:
        fail("inner callback abort")
def outer():
    for x in S:
        CAUGHT.append(catch_inner(inner, mutation))
outer()
CAUGHT == [True, True] and list(S) == [1,2]
"#
            );
            assert_eq!(iterator_eval(&mut eval, &source).unwrap().to_repr(), "True");
            assert_recovered_on_same_evaluator(&mut eval, mutation);
            drop(eval);
            assert_recovered_on_fresh_evaluator(&module, fresh_mutation);
        });
    }
}

#[test]
fn retained_rust_iterator_survives_inner_vm_error_until_its_own_drop() {
    for (constructor, mutation, fresh_mutation) in ITER_CONTAINERS {
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            // A Rust-held Value/iterator is not a GC root. This test is strictly
            // about ownership of iteration guards, not live-iterator GC safety.
            eval.disable_gc();
            iterator_eval(&mut eval, &format!("S={constructor}\ndef inner():\n    for x in S:\n        fail('host-held inner abort')\n")).unwrap();
            let mut held = module.get("S").unwrap().iterate(module.heap()).unwrap();
            assert_eq!(held.next().unwrap().to_repr(), "1");
            let error = eval
                .eval_function(module.get("inner").unwrap(), &[], &[])
                .unwrap_err();
            assert!(error.to_string().contains("host-held inner abort"));
            let error = iterator_eval(&mut eval, mutation)
                .expect_err("VM error released host-owned iterator");
            diagnostic(&error, &["iterat", "immutable"], mutation);
            assert_eq!(held.next().unwrap().to_repr(), "2");
            // Still acquired: next() has not yet observed exhaustion.
            let error = iterator_eval(&mut eval, mutation).unwrap_err();
            diagnostic(&error, &["iterat", "immutable"], mutation);
            drop(held);
            assert_recovered_on_same_evaluator(&mut eval, mutation);
            drop(eval);
            assert_recovered_on_fresh_evaluator(&module, fresh_mutation);
        });
    }
    // Host RAII exhaustion also stops exactly once, including repeated next()
    // after None followed by Drop; the counted witness detects double release.
    for len in [0, 2] {
        Module::with_temp_heap(|module| {
            let counts = Arc::new(Mutex::new(host_values::IterCounts::default()));
            let value = module
                .heap()
                .alloc_complex_no_freeze(host_values::CountedIterable {
                    len,
                    counts: counts.clone(),
                });
            let mut held = value.iterate(module.heap()).unwrap();
            for _ in 0..len {
                assert!(held.next().is_some());
            }
            assert!(held.next().is_none());
            assert!(held.next().is_none());
            drop(held);
            let counts = counts.lock().unwrap();
            assert_eq!((counts.starts, counts.stops, counts.active), (1, 1, 0));
        });
    }
}

#[test]
fn cancellation_at_loop_backedge_releases_nested_guards_and_evaluator_remains_usable() {
    for (constructor, mutation, fresh_mutation) in ITER_CONTAINERS {
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.disable_gc();
            iterator_eval(&mut eval, &format!("S={constructor}\nENTERED=[]\n")).unwrap();
            let cancellations = Arc::new(AtomicUsize::new(0));
            let observed = cancellations.clone();
            let module_ref = &module;
            eval.set_check_cancelled(Box::new(move || {
                let inside = module_ref.get("ENTERED").unwrap().to_repr() != "[]";
                if inside {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                inside
            }));
            let source = "def f():\n    for x in S:\n        for y in S:\n            ENTERED.append(True)\n            for busy in range(1000000):\n                pass\nf()\n";
            let error = iterator_eval(&mut eval, source).unwrap_err();
            diagnostic(&error, &["cancel"], source);
            assert!(cancellations.load(Ordering::SeqCst) > 0);
            assert_eq!(module.get("ENTERED").unwrap().to_repr(), "[True]");
            eval.set_check_cancelled(Box::new(|| false));
            assert_recovered_on_same_evaluator(&mut eval, mutation);
            drop(eval);
            assert_recovered_on_fresh_evaluator(&module, fresh_mutation);
        });
    }
}

#[test]
fn tick_budget_error_unwinds_live_containers_without_resetting_resource_policy() {
    for (constructor, mutation, fresh_mutation) in ITER_CONTAINERS {
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.disable_gc();
            iterator_eval(&mut eval, &format!("S={constructor}\nENTERED=[]\n")).unwrap();
            eval.set_max_tick_count(10000).unwrap();
            let source = "def f():\n    for x in S:\n        ENTERED.append(True)\n        for busy in range(1000000):\n            pass\nf()\n";
            let error = iterator_eval(&mut eval, source).unwrap_err();
            diagnostic(&error, &["ticks has been exceeded"], source);
            assert_eq!(module.get("ENTERED").unwrap().to_repr(), "[True]");
            // This limit cannot be reset; using a fresh evaluator is intentional,
            // rather than requiring cleanup to bypass the exhausted tick policy.
            drop(eval);
            let mut fresh = Evaluator::new(&module);
            assert_recovered_on_same_evaluator(&mut fresh, mutation);
            drop(fresh);
            assert_recovered_on_fresh_evaluator(&module, fresh_mutation);
        });
    }
}

struct FailAtStatement {
    fired: Arc<AtomicUsize>,
}

impl<'e> BeforeStmtFuncDyn<'e> for FailAtStatement {
    fn call<'v>(
        &mut self,
        span: FileSpanRef,
        continued: bool,
        _: &mut Evaluator<'v, '_, 'e>,
    ) -> starlark::Result<()> {
        if !continued && span.source_span().starts_with("INSTRUMENT_FAIL") {
            self.fired.fetch_add(1, Ordering::SeqCst);
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "statement hook abort"
            )));
        }
        Ok(())
    }
}

#[test]
fn before_statement_failure_unwinds_nested_guards_on_same_and_fresh_evaluators() {
    for (constructor, mutation, fresh_mutation) in ITER_CONTAINERS {
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.disable_gc();
            let fired = Arc::new(AtomicUsize::new(0));
            // Use the existing exported DAP hook, installed before compilation;
            // no private production hook or new instrumentation API is required.
            eval.before_stmt_for_dap(BeforeStmtFunc::from_dyn(Box::new(FailAtStatement {
                fired: fired.clone(),
            })));
            let source = format!(
                "S={constructor}\nENTERED=[]\ndef f():\n    for x in S:\n        for y in S:\n            ENTERED.append(True)\n            INSTRUMENT_FAIL=x\nf()\n"
            );
            let error = iterator_eval(&mut eval, &source).unwrap_err();
            diagnostic(&error, &["statement hook abort"], &source);
            assert_eq!(fired.load(Ordering::SeqCst), 1);
            assert_eq!(module.get("ENTERED").unwrap().to_repr(), "[True]");
            assert_recovered_on_same_evaluator(&mut eval, mutation);
            drop(eval);
            assert_recovered_on_fresh_evaluator(&module, fresh_mutation);
        });
    }
}
