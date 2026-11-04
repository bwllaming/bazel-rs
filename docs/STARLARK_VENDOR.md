# Pinned Starlark vendor provenance

## Tab whitespace parser parity (Bazel 9.3)

The lexer lives in the separate, already-resolved `starlark_syntax = 0.14.2`
package, not in `starlark`. The root patch table now also selects
`vendor/starlark_syntax-0.14.2`. Its lock entry loses only its registry source
and checksum; there are no new dependency packages, versions, features, or
dialect changes. Shared Cargo/rustup source directories are not patched.

The published archive is
<https://crates.io/api/v1/crates/starlark_syntax/0.14.2/download>, SHA-256
`7492c571c531e68099c911cfd909d32659f1cc0910cf3adee9fce66e39d21f14`
(the prior lock checksum). Its VCS metadata names the same upstream commit
`0c8d107ab963362ba47ee3ab9e0173bc39bd101d`. All 274 published regular files
(1,227,338 bytes) are preserved. `LICENSE` is the sole additional file, copied
byte-for-byte from the existing vendor's authentic license at that same commit.
`vendor/starlark_syntax-0.14.2.provenance.json` anchors the original inventory,
license, and the single patched member, `src/lexer.rs`, which carries a local
modification notice. `tests/starlark_tabs.rs` reconstructs and verifies that
inventory without a registry, network, or untracked artifact.

Bazel's `src/main/java/net/starlark/java/syntax/Lexer.java`
(`computeIndentation`, `newlineInsideExpression`, `tokenize`, `escapedStringLiteral`)
and the genuine 9.3 ELF capture in
`tests/fixtures/starlark_tabs/reference_receipt.json` establish the boundary:

* Tabs between tokens, after semicolons/inline-suite colons, and in implicit
  (`()`, `[]`, `{}`) or explicit backslash continuations are whitespace.
* Tabs while computing indentation are errors, including mixed spaces/tabs,
  blank lines, comment-only lines, and trailing whitespace at EOF. The tab
  error points immediately after the first tab, as genuine Bazel does, and
  reads `Tab characters are not allowed for indentation. Use spaces instead.`
* Literal tabs in ordinary/raw/triple strings and `\t` escapes keep their
  values. Only backslash + **literal tab** in a non-raw string is newly rejected
  as an invalid escape; unrelated escape/Unicode/byte-string behavior is not
  changed. CRLF continuations work. **Known baseline divergence:** genuine
  preserves triple-string CRLF values; native drops CR with or without tabs.
  An isolated evaluator using the unpatched registry parser reproduces this
  (`a\r\n\tb` -> bytes `[97, 10, 9, 98]`; without tab -> `[97, 10, 98]`).
  This is explicitly pinned as unchanged, not reported as value parity.

The lexer consumes whitespace directly; it never replaces source characters,
rewrites files, changes a codemap, or expands columns to tab stops. Other
indent/dedent validation and error spans are unchanged. The 37 `.bzl` probes
pin 22 accepted cases (21 matching executed strings and the CRLF baseline
divergence) and 15 rejected cases, including ordinary
syntax errors after accepted tab separators. A generated-style BUILD probe
also succeeds with the original tab-containing attribute. Native CLI tests
cover MODULE, `.bzl` loads/macros, BUILD, `use_repo_rule`, and repository-generated
BUILD loading, and verify that source bytes remain unchanged. General parser
diagnostic wording (e.g. invalid escapes and non-tab dedents) is not made identical
to Bazel here; the tab diagnostic and post-tab token positions are pinned.

## Interpreter vendor

The root `[patch.crates-io]` selects `vendor/starlark-0.14.2`. The dependency
remains exactly `starlark = "=0.14.2"`; H0 changed its lock entry only by removing
that package's registry source and checksum. The original H0 baseline changed no
semantics. Subsequent [set-core compatibility patches](STARLARK_SETS.md) are
declared separately; they do not upgrade dependencies. Production globals are
enabled by the subsequent native integration, not by the vendored-core patch
itself.

The native JSON codec adds one typed, allocation-free BigInt borrow accessor to
the vendored Value API, permitting budget checks before integer conversion. Its
patch and tests are recorded separately in the same provenance. The root crate
also adds a direct dependency on the already-resolved `num-bigint = "=0.4.8"`;
dependency versions and enabled features remain unchanged.

[String-method and `repr` patches](STRING_METHODS.md) for Bazel 9.3 byte-string
semantics modify `src/values/types/string/{methods,repr,globals}.rs`; they are
declared in the same provenance with their original archive hashes.

[Location-aware indexing and `in`](TOOLCHAINS.md) for native `ctx.toolchains`
string keys add two `StarlarkValue` methods with defaults,
`at_with_location(index, heap, location)` and `is_in_with_location(other,
location)`, which delegate to `at` and `is_in`. `location` lazily returns the
span of the indexing or `in` expression (its own module, also when the
expression was inlined from another module's function). Only the bytecode
`ArrayIndex` and `In` instructions call them (`src/eval/bc/instr_impl.rs`;
`In` becomes a no-flow instruction with the same argument slots); the vtable
gains the two entries (`src/values/layout/vtable.rs`, `src/values/traits.rs`).
Other values keep a single vtable call; the span is computed only when a value
asks for it. An A/B release benchmark of 4,000,000 iterations of list/dict
indexing and `in` over the pristine and patched crates measured 0.326-0.333 s
(pristine) versus 0.303-0.307 s (patched), i.e. no regression. The vendored
crate's own tests cannot run in this workspace (`starlark` is not a member and
needs dev-dependencies); the behaviour is covered by `src/toolchain_info.rs`
unit tests and `tests/toolchains.rs`.

The [label-dictionary increment](ATTR_DICTS_COMPUTED_DEFAULTS.md) (N20) patches
`src/values/{error,traits}.rs`, `src/values/layout/{value,vtable}.rs`,
`src/values/types/{list/value,dict/refs,dict/value}.rs` and
`src/eval/compiler/stmt.rs`: mutating a frozen list or dict reports Bazel's
`Starlark.checkMutable` text (`net/starlark/java/eval/Starlark.java:262-265`), `trying to mutate a frozen list value` /
`trying to mutate a frozen dict value` (other immutable values keep
`Immutable`), and a new `StarlarkValue::rbit_or` hook (default `None`) lets a
non-dict right operand of a dict `|` / `|=` answer first, as Bazel's
`SelectorList.binaryOp(op, that, thisLeft = false)` does for
`{...} | select(...)` (`packages/SelectorList.java:171-182`). These files are
declared with their original archive hashes in the same provenance. The
vendored crate's own tests that asserted the former list/dict text are updated
to the Bazel text as declared patches: `src/tests/call.rs`,
`src/tests/uncategorized.rs` (`test_error_display`),
`src/values/types/dict/methods.rs`, `src/values/types/list/value.rs`, and the
two error-display goldens `src/tests/uncategorized_error_display{,_hash}.golden`
(plain and `{:#}` renderings; only the `error:` line changes, and their notice is
in `BAZEL_RS_MODIFICATIONS.md` because the golden format cannot carry one).
Tuple, bytes and set mutation tests still expect `Immutable`.

[repr/str/debugPrint patches](REPR.md) for Bazel 9.3 value printing modify
`src/values/types/{structs/value,function}.rs`, `src/environment/globals.rs`,
`src/values/types/string/{interpolation,dot_format}.rs`,
`src/eval/compiler/def.rs`, `src/values/{traits,layout/vtable}.rs` and
`src/stdlib/{extra,funcs/other}.rs` and `src/values/error.rs` (Bazel's
`unhashable type: 'T'` text); each carries an inline modification notice and is
declared in the same provenance with its original archive hashes. The crate's
own tests follow those behaviours: `src/environment/modules.rs`,
`src/values/layout/value.rs`, `src/tests/{basic,uncategorized,fstring,opt/def_inline}.rs`,
`src/values/types/set/{methods,set,value}.rs`, the `format` doc test in
`src/values/types/string/methods.rs`, `testcases/eval/go/function.star` (inline
notices) and 29 `.golden` files regenerated with
`STARLARK_RUST_REGENERATE_GOLDEN_TESTS=1` (listed in `BAZEL_RS_MODIFICATIONS.md`,
whose declared-addition hash is updated). Under the frozen isolated test graph
the vendored crate passes 964 lib tests and 157 doc tests (17 ignored).

The [runfiles increment](RUNFILES.md) (N23a) adds
`StarlarkValue::unknown_attribute_message(attribute)` (default `None`) in
`src/values/{traits.rs,layout/vtable.rs}`; attribute reads
(`src/eval/compiler/expr.rs`, `get_attr_no_attr_error`) and
`Value::get_attr_error` (`src/values/layout/value.rs`) use it before the
ordinary `has no attribute` error, so `ctx.outputs` and `ctx.executable` report
Bazel's `Structure.getErrorMessageForUnknownField` texts
(`analysis/starlark/StarlarkRuleContext.java:402-408`,
`analysis/starlark/StarlarkAttributesCollection.java:82-86`,
`packages/StructImpl.java:86-95`). `src/eval/compiler/expr.rs` is declared
with its original archive hash in the same provenance.

The [N26 string increment](STRING_METHODS.md) patches `src/eval.rs`,
`src/values/types/string.rs` and `src/values/types/string/methods.rs`:
`Evaluator::eval_module` rewrites string literal values per Bazel 9.3's lexer
(`net/starlark/java/syntax/Lexer.java:280-430`) before compilation (raw strings
keep the backslash before a quote; in the default analysis model a literal with a
`\200`..`\377` byte escape keeps lone bytes as private-use characters
U+10FF00 + byte, `eval::bazel_byte_char`), a per-thread
Latin-1 char model (`enter_latin1_char_model`, re-exported from
`values::string`) selects Bazel's Java char semantics for `isspace`, `title` and
`capitalize`, and `split`/`rsplit` require a string `sep`
(`StringModule.java:387-468`) while `elems()` returns a list (`:926-940`).
Upstream doc examples and one unit test that asserted the replaced behaviour were
updated with modification notices; three tests were added. The isolated vendored
test run (frozen h1 test-graph lock, direct 1.99.0 binaries, offline;
`/tmp/opencode/n26v-scratch/receipt.json`) showed, on N26's own base, the same
five pre-existing lib failures as its unmodified base tree (whose vendored tests
still expected the pre-N20 `Immutable` text) and no new failure. On the merged
tree (with the N20 vendored test expectations, N22, N23a and N26) the same
procedure passes all 967 lib tests and 157 doc tests (17 ignored).

[Starlark transitions](CONFIGURATION.md) (N18c) use the same N23a hook (the
N18c branch carried identical hunks; one copy is kept). N18c additionally makes
`getattr(x, name)` without a default consult the hook
(`src/stdlib/funcs/other.rs`), as Bazel's `Starlark.getattr` uses
`Structure.getErrorMessageForUnknownField`
(`net/starlark/java/eval/Starlark.java:1002-1008`). The transition `attr`
struct uses it for Bazel's `No attribute '%s'. ...` texts with
`Available attributes: ...`. The patched file carries an inline notice and its
current hash is updated in the provenance; no vendored test changes.

The [N33 interpreter-wording increment](#n33-interpreter-error-texts) gives
the most common interpreter errors Bazel 9.3's `net.starlark.java` texts.

The [N48 recursion increment](#n48-recursive-calls) rejects recursive calls
like Bazel.

### N33 interpreter error texts

Sources are the pinned Bazel tree (`src/main/java/net/starlark/java/eval/`):

- **`def` binding** (`StarlarkFunction.java:385-523`): `NAME() does not accept
  positional arguments, but got N`, `NAME() accepts no more than K positional
  argument(s) but got N`, `NAME() got unexpected keyword argument(s): a, b` with
  ` (did you mean 'x'?)` only for exactly one, `NAME() missing N required
  positional|keyword-only argument(s): a, b` (all listed) and `NAME() got
  multiple values for parameter 'x'`. `NAME` is the def's own name (`lambda` for
  lambdas). Bazel's order is replayed: the call adds explicit positional
  arguments, explicit named ones, `*args` elements, then `**kwargs` entries
  (`Eval.java:716-756`); duplicates fail immediately; then the positional count,
  unexpected names and missing parameters (`call()`). As in Bazel, `*args`
  elements bind after the named arguments and overwrite a parameter already
  given by name (`g(a = 1, *[2])` binds `a = 2`; `addPositionalArg` has no
  duplicate check, StarlarkFunction.java:460-472, BuiltinFunction.java:307-319);
  only explicit positional arguments and `**kwargs` entries clash.
- **Builtin binding** (`BuiltinFunction.java:150-160, 307-430, 470-490`):
  `NAME() got unexpected keyword argument 'x'` (+ hint over all parameters),
  `NAME() got named argument for positional-only parameter 'x'`, `NAME() got
  multiple values for argument 'x'` / `for keyword argument 'x'`, `NAME() got
  unexpected positional argument`, `NAME() accepts no more than ...`, `NAME()
  missing N required positional|named argument(s): ...`. `NAME` is the method
  name. Positional-only builtins bound by generated code
  (`Arguments::optional`/`no_named_args`), `set()` and `dict()` are rebound
  against their spec on the error path (`ParametersSpec::bazel_rebind_error`).
  A parameter of the wrong type gives `in call to NAME(), parameter 'P' got value
  of type 'T', want 'W'` (`BuiltinFunction.java:404-410`); like Bazel's
  `checkParamValue`, a binding error is replayed with each argument's class
  checked as it binds, so a wrong type wins over the positional count
  (`len(1, 2)`, `depset(1, 2, 3)`; native parameter types are kept on
  `NativeFunc`/`NativeMeth`, with Bazel's declarations for the unchecked `len`,
  `enumerate` and `depset` parameters); `W` derives from the
  starlark-rust type (`str` -> `string`, `None` -> `NoneType`, lists ->
  `sequence`, `string` first, `NoneType` last, `a, b, or c` form,
  `ParamDescriptor.java:122-145`); a container of the right type with wrong
  elements keeps the detailed text. `list`, `tuple`, `set`, `sorted`,
  `reversed`, `any` and `all` check their iterable like Bazel, and `len`,
  `hash`, `getattr`, `hasattr`, `enumerate`, `reversed`, `sorted`, `any` and
  `all` use Bazel's parameter names. All string, list and dict methods and the
  `MethodLibrary` builtins were audited against Bazel's `@Param` names and
  kinds (StringModule.java, StarlarkList.java, Dict.java, MethodLibrary.java):
  `find`/`rfind`/`index`/`rindex`/`count`/`startswith`/`endswith` take a
  positional-only `sub` (so `sub = ` and `needle = ` both fail like Bazel),
  `partition`/`rpartition` `sep`, `join` `elements` (with Bazel's `expected
  string for sequence element N` text), `range(start_or_stop, stop, step)`,
  list `append`/`insert` `item`, `extend` `items`, `index`/`remove` `x`, `pop`
  `i`, `tuple`/`list`/`str`/`repr`/`float`/`type` `x`, `int` a required `x`;
  `dict.get`/`setdefault` take `default` by name, `sorted`'s `key` and
  `enumerate`'s `list` may be positional or named, and `fail` (`msg`, `attr`,
  `sep`, `stack_trace`) and `print` (`sep`) gain Bazel's named-only
  parameters. Approximation: `fail(stack_trace = False)` is accepted, but the
  native error still prints its traceback (Bazel omits it,
  `MethodLibrary.java:891-914`).
- **Call arguments** (`Eval.java:735, 749, 754`): `argument after * must be an
  iterable, not T`, `argument after ** must be a dict, not T`, `keywords must be
  strings, not T`.
- **Fields** (`Starlark.java:1002-1017`, `EvalUtils.java:526-535`):
  `'T' value has no field or method 'x'` with `SpellChecker.didYouMean` over the
  sorted `dir(x)` (a port in `src/errors/did_you_mean.rs`); `struct()` values,
  custom provider instances and `ToolchainInfo` list `Available attributes:`
  instead (`StructImpl.java:85-95`); `cannot set .x field of T value`, and
  `T value does not support field assignment` for structures.
- **Operators**: `unsupported comparison: T1 <=> T2` (`Starlark.java:517`),
  `unsupported binary operation: T1 OP T2` (`EvalUtils.java:416-417`, also for
  `in`), `unsupported unary operation: OPT` (`:472`), `type 'T' has no operator
  [](K)` (`:498-500`), `'T' object is not callable` (`Starlark.java:904`), `type
  'T' is not iterable` (`:284`), `invalid slice operand: T` (`:672`), `'in
  <string>' requires string as left operand, not 'T'` (`EvalUtils.java:382-386`)
  and `dictionary expression has duplicate key: REPR` (`Eval.java:650`).
- **Ordering** (`Starlark.java:492-518`): only bool, int/float, string,
  list, tuple, `Label` and `File` (Artifact by exec path, `Artifact.java:134-145,
  197-199`; `src/starlark_types.rs`) are ordered; `struct()` values are no
  longer ordered (`unsupported comparison: struct <=> struct`), like provider
  instances, depsets, functions, dicts, sets, ranges and `None`.
- **Names**: `name 'x' is not defined` with the SpellChecker hint
  (`Module.java:250-256`, `syntax/Resolver.java:941-952`). Approximation: Bazel
  iterates its candidates in `HashSet` order and keeps the first of equally
  close names; natively the candidates are sorted, so a tie resolves to the
  first in sorted order, which may differ from Bazel's choice.
- **One spelling checker**: the SpellChecker port (`bazel_suggest`,
  `bazel_did_you_mean`, `bazel_edit_distance`, over UTF-16 code units like Java
  strings) is public in `starlark::errors`; bazel-rs uses it for native rule
  attributes, `extension_metadata`, guarded globals, target suggestions and
  option suggestions instead of its former local ports (one of which counted
  UTF-8 bytes).
- **Sequences**: Bazel's `Sequence` includes ranges (`RangeList`); `depset`'s
  `direct`/`transitive` and `Args.add_all`/`add_joined` `values` accept
  `range(...)` (`src/starlark_types.rs` `bazel_sequence`). Parameters whose
  elements must be strings, labels, files or providers still report a
  container error for a range where Bazel reports the element.

Every changed vendored file carries an inline notice and is declared in the
provenance (103 declared patches, 30 new); the three regenerated goldens are
listed in `BAZEL_RS_MODIFICATIONS.md`. The crate's own tests that asserted the
replaced texts now assert the Bazel texts. Under the frozen isolated test graph
(`/tmp/opencode/n33-vendor-tests/run.py`, the N22 procedure: copied tree, frozen
h1 test lock, direct 1.99.0 binaries, offline) the vendored crate passes 968 lib
tests and 157 doc tests (17 ignored). A paired capture against genuine Bazel
9.3.0 (`tests/fixtures/interpreter_wording/reference_receipt.json`) matched 113 of
121 load-time probes in run 1; after the follow-up (ordering, `*args` binding,
binding-time type checks, parameter names, a fixed probe) run 2 matches all
168 probes (157 identical messages, 11 identical `print` outputs; run 1 is kept
in `run_history`, no changes after run 2). `File` ordering is covered by unit
tests only (the capture evaluates at load time). Static syntax errors from `starlark_syntax` (`repeated named
argument`) and the traceback layout are unchanged.

### N48 recursive calls

Bazel's Starlark has no recursion (`StarlarkSemantics.ALLOW_RECURSION`,
`-allow_recursion`, is `false` and not exposed by Bazel). After binding a
`def` or `lambda` call's arguments (so binding errors win), Bazel fails with
`function 'NAME' called recursively` when the function's *code* is already on
the thread's call stack (`StarlarkFunction.java:571-574`,
`StarlarkThread.isRecursiveCall`, `:408-420`, which compares the resolved
function, not the closure, "otherwise one can defeat the check by writing the
Y combinator"). The error is raised inside the callee's frame, so the innermost
traceback frame is the function's location: the `def` identifier or the start
of the `lambda` (`syntax/Resolver.java:825-850`).

The vendored interpreter now does the same (`src/eval/compiler/def.rs`
`invoke_raw`, `src/eval/runtime/evaluator.rs` `enter_def`/`exit_def`): the
evaluator keeps a (boxed) stack of the `DefInfo` pointers of executing
`def`/`lambda` calls (one `DefInfo` per `def` statement or `lambda`
expression, shared by all closures) and rejects a call whose `DefInfo` is
already on it, so
direct and mutual recursion, a lambda calling itself, a nested `def` calling
its enclosing function, two closures of one nested `def`, and recursion
through builtin callbacks (`sorted(key = ...)`, `max(key = ...)`, a
provider `init` calling its provider) are all rejected; builtins are not
tracked. Functions whose bodies are inlinable (below) call nothing that can
run Starlark code, so they can never be part of a cycle and are not tracked
either.
The error carries `DefInfo.identifier_span`, so native's `-->` location is
Bazel's innermost frame (`def f(` -> column of `f`, `lambda` -> its keyword) and
the traceback above it ends at the recursive call. `NAME` is the def's name or
`lambda`. Calling a function again after it returned, or functions that refer
to each other without dynamically recurring, still work.

An inlined function has no call stack frame, so the check could not see it:
only function bodies whose calls cannot run Starlark code are still inlined
(`src/eval/compiler/def_inline.rs`): calls of global builtins that never call
an argument (`len`, `str`, `type`, `getattr`, `list`, `dict`, ... but not
`sorted`/`min`/`max`, which take `key`) and methods of string constants
(`",".join(x)`), without function-valued constant arguments. Bodies such as
`return len(x)`, `return [str(a), str(b)]` or `return type(x) == "list"` are
inlined as before; `return g(x)`, `return x.upper()` (the receiver may be a
struct with a function field) or `return sorted(x, key = k)` are not.

Two embedder APIs: `Evaluator::set_allow_recursion(bool)` (Bazel's
`-allow_recursion`, default `false`; used only by the crate's own tests) and
`Evaluator::with_new_recursion_scope(f)`, under which frames already on the
stack do not count. bazel-rs uses the latter where Bazel runs a callback in a
new `StarlarkThread` but native evaluates it nested in the caller's evaluator:
symbolic macro implementations (`src/package.rs`, `MacroClass.java:390-420`),
computed defaults (`src/custom_rule.rs`, `StarlarkCallbackHelper.java:66-70`)
and `map_each` of `Args` and `TemplateDict` (`src/args.rs`,
`src/template_dict.rs`; `StarlarkCustomCommandLine.java:1138`,
`TemplateDict.java:125`). Rule implementations, aspects, transitions,
repository rules and module extensions already run in their own evaluator.
A static scan of native-owned Starlark (the embedded `@bazel_tools` archive,
`resources/builtins_bzl`, Starlark in Rust sources and test fixtures) found no
dynamic recursion; the full Rust and Python suites pass.

The crate's own tests follow: `funcall_test` (`src/tests/call.rs`) expects the
Bazel text for `rec1()`/`rec2()` and keeps its call-stack-overflow checks with
recursion allowed; the Go `function.star` chunk marked `option:recursion`
(Go's Starlark also rejects recursion by default) runs with recursion allowed,
and is also checked to fail without it (`src/tests/go.rs`); two `def_inline`
goldens were regenerated (listed in `BAZEL_RS_MODIFICATIONS.md`);
`src/tests/call.rs` adds three tests (locations, traceback rendering,
`set_allow_recursion` and `with_new_recursion_scope`) and
`src/tests/opt/def_inline.rs` one (which bodies are inlined). The provenance
now declares 105 patches (2 new: `src/eval/compiler/def_inline.rs`,
`src/tests/go.rs`). Under the frozen isolated test graph
(`/tmp/opencode/n48-vendor-tests/run.py`, the N22 procedure) the vendored
crate passes 972 lib tests and 157 doc tests (17 ignored). CLI tests are in
`tests/no_recursion.rs`. `tests/test_native_starlark_recursion.py` makes the
static scan reproducible: it builds a per-file static call graph of
native-owned Starlark (the embedded `@bazel_tools` archive, `resources/`,
`tests/fixtures` files and archives, Starlark in Rust string literals of `src/`
and `tests/`) and fails on any function that can reach itself, except an
allowlist (the N48 CLI probes, upstream `cc_import.bzl`, skylib's static-only
`selects.bzl` cycle, one Rust format template), which must not go stale.

Performance (release builds of 3d09a66 and this change, interleaved runs,
median user+sys CPU of the child, private caches; `/tmp/opencode/n48-perf`):
warm native `query` of `~/code` `//src/beyondwork.ai/utils/...` and
`//src/beyondwork.ai/langchaingo/...` (`--lockfile_mode=off`, BCR registry,
private `--native-lockfile`) and the N44-style 160-package fixture are within
measurement noise (-2.2%..+3.2% over 11- and 21-run sets); a synthetic
workspace of about 36,000,000 small helper calls (100 packages x 10 macros x
4,000 iterations of nine helpers) is +1.4% (21 runs) and 40,000,000 calls of
a trivial `def` (`a + b`) +2.8% (11 runs). A first version that walked the call
stack and inlined no body with calls cost +11.5% on the helper workload;
tracking `DefInfo` pointers, not tracking inlinable bodies and inlining
non-calling builtin bodies brought it to the numbers above (an unboxed
evaluator field alone cost about 7% through layout).

A paired capture against genuine Bazel 9.3.0
(`tests/fixtures/no_recursion/reference_receipt.json`, 23 probes: `.bzl` top
level, legacy and symbolic macros, rule initializer, rule implementations)
matches the message and the innermost location (the re-entered function) of
all 16 recursion errors Bazel reports; 12 also match every traceback frame and
2 rule-implementation probes every Starlark frame (native's traceback layout
adds host-callback builtin frames and shows provider/macro callables as
builtin frames). The 3 probes Bazel accepts (no dynamic recursion, a helper
re-entered by a symbolic macro implementation instantiated from a legacy
macro, `map_each` re-entering the function that registered it) succeed
natively with the same output; `T/binding_error_first` matches its binding
message. Pre-existing gaps the capture records and N48 does not change: rule
instantiation inside a rule initializer is not rejected natively (Bazel: `a
rule can only be instantiated while evaluating a BUILD file or a legacy or
symbolic macro`; natively the initializer then re-enters itself and is
reported as recursive), nested symbolic macros are unsupported natively, and
Bazel's symbolic macro instantiation recursion check (`macro 'x_x' is a direct
recursive call of 'x'`) is not implemented. Binding errors keep their call-site
location (N33).

### Portable `def` and `lambda` values (analysis snapshot, P1)

The analysis snapshot saves a plan whose Args `map_each` functions run at
execution time (docs/ANALYSIS_SNAPSHOT.md, "Args map_each functions") by
describing each function and building it again in the process that hits the
snapshot. Four vendored files carry the support (each with a header notice):

* `src/environment/modules.rs`: every `Module` keeps the `DefInfo` of each
  `def` and `lambda` it compiled (`ModuleDefInfos`, in compilation order;
  `FrozenModuleData::def_infos` after the freeze, skipped by the pagable
  serialization), and `FrozenModule::data()` gives the module data a frozen
  `def` refers back to.
* `src/eval/compiler/def.rs`: `Compiler::function` registers the `DefInfo` it
  allocates; `PortableDef::of(value)` takes a frozen `def`/`lambda` apart —
  its name, file and identifier span, the parameter list with its default
  values, and the captured variables of a nested function (functions with
  type annotations are refused) — and `PortableDef::instantiate(defining,
  module)` builds the function again from the `DefInfo` the `defining` module
  compiled at that name, file and span, with the given defaults and captured
  values (fresh captured cells), its globals those of `defining` (the
  module back-reference a `def` instruction would set; the body is optimized
  on freeze as for any frozen `def`). `def_same_code(a, b)` (same `DefInfo`
  and module) and `def_module_is(value, module)` support the embedder's
  checks.
* `src/eval/runtime/params/spec.rs`: `ParametersSpec::portable_parts` and
  `from_portable_parts`, the parameter list taken apart and rebuilt exactly
  as the `def` instruction (`InstrDefImpl`) builds it; a list no `def` has
  is an error, never a panic of the builder's assertions.
* `src/eval.rs`: the public re-exports.

Nothing changes for evaluation itself: the registry is one pointer per
compiled function, and no existing code path reads it.

## Vendored `starlark_syntax` (parser)

The parser lives in the separate `starlark_syntax` crate, vendored as
`vendor/starlark_syntax-0.14.2` (see "Tab whitespace parser parity" above).
A second declared patch in
`vendor/starlark_syntax-0.14.2.provenance.json` (original and current size
and sha256, verified by `tests/starlark_tabs.rs`) changes the parser.

Its changes (`src/syntax/parser_rd.rs`, each with an inline
`Modified (bazel-rs)` notice) make the parser take an unparenthesized tuple
wherever Bazel's grammar takes `expr` (`Parser.java` `parseExpr`). Upstream
took a single `test` at some of these positions:

* a `for` iterable: `for key in "GONOPROXY", "GONOSUMDB":`, as in
  bazel-gazelle `internal/env.bzl:223` (`parseForStatement`);
* a subscript with any number of elements, `d["x", 1, 2]`, which indexes
  with the tuple (`parseSliceSuffix`). Upstream's two-element `Index2` node
  (`at2`) remains only in dialects with type expressions, where
  `dict[str, int]` needs it. The Bazel dialects disable types, so
  `d["y", 2] = v` is also a valid assignment target there;
* an expression statement `"a", "b"` (`parseSmallStatement`).

The return value and both sides of an assignment already took the tuple.
A trailing comma stays an error at all these positions, as in Bazel. The
behaviour is pinned by `tests/bench_parity.rs`.

## Original published contents

The source archive is
<https://crates.io/api/v1/crates/starlark/0.14.2/download>, SHA-256
`9062e866918dc4c9701c98ac99f7f4fa9e4b3b4edce306e147393bc75458c4fc`.
Its `.cargo_vcs_info.json` identifies upstream commit
`0c8d107ab963362ba47ee3ab9e0173bc39bd101d` in
<https://github.com/facebook/starlark-rust>.

H0 preserved all 685 regular archive members (3,535,949 bytes), including the
published manifests, VCS metadata, tests and golden files. All paths remain
present. Each later source patch records original and current sizes/hashes;
modified files retain their license headers and carry modification notices.
The original archive and inventory digests are never replaced by patched-tree
digests. Unlisted changes or additions fail verification.

`vendor/starlark-0.14.2.provenance.json` records the origins and explicitly
distinguishes the additional license from published archive members. Its
archive-member digest binds the complete, lexically ordered per-path size/SHA-256
mapping. It is reproducible directly from the tracked tree; no ignored receipt,
network request or registry cache is required. The optional expanded inventory
is in `artifacts/starlark-set-vendor-baseline.json`.

From the repository root, verify current files and reconstruct the anchored
original inventory using only declared patches:

```sh
python3 - <<'PY'
import hashlib, json
from pathlib import Path
p = json.loads(Path('vendor/starlark-0.14.2.provenance.json').read_text())
root = Path(p['archive']['archive_member_inventory']['path_base'])
extra = {v['path']: v for v in p['additional_files']}
patches = {v['path']: v for v in p['declared_source_patches']}
assert len(extra) == len(p['additional_files'])
assert len(patches) == len(p['declared_source_patches'])
assert not set(extra).intersection(patches)
members, found_extra, found_patches = [], set(), set()
for f in sorted(root.rglob('*'), key=lambda f: f.relative_to(root).as_posix()):
    assert not f.is_symlink(), f
    if f.is_dir():
        continue
    assert f.is_file(), f
    data = f.read_bytes()
    item = dict(path=f.relative_to(root).as_posix(), size=len(data),
                sha256=hashlib.sha256(data).hexdigest())
    if item['path'] in extra:
        assert all(extra[item['path']][k] == v for k, v in item.items())
        found_extra.add(item['path'])
    else:
        if item['path'] in patches:
            patch = patches[item['path']]
            assert all(item[k] == patch['current'][k] for k in ('size', 'sha256'))
            found_patches.add(item['path'])
            item = dict(path=item['path'], **patch['original'])
        members.append(item)
assert found_extra == set(extra)
assert found_patches == set(patches)
assert len(members) == p['archive']['regular_members']
assert sum(v['size'] for v in members) == p['archive']['uncompressed_regular_file_bytes']
raw = json.dumps(members, sort_keys=True, separators=(',', ':')).encode()
assert hashlib.sha256(raw).hexdigest() == p['archive']['archive_member_inventory']['canonical_json_sha256']
print('Original inventory, declared patches and declared additions verified')
PY
```

## Authentic license addition

The published archive has no LICENSE or NOTICE file, despite its per-file
Apache headers. `vendor/starlark-0.14.2/LICENSE` is an unmodified copy fetched
from the official repository at the exact source commit:

<https://raw.githubusercontent.com/facebook/starlark-rust/0c8d107ab963362ba47ee3ab9e0173bc39bd101d/LICENSE>

It is 11,324 bytes, SHA-256
`50e6751797c50dedd75ef1b8a0d9e42f5f8472e9fbce91f34718e9f97b0c780a`.
This is an explicit addition, not an archive member. The pinned upstream
NOTICE request returned HTTP 404 and the complete pinned tree contained no
NOTICE, so none is fabricated. Acquisition evidence is in the `licensing`
section of `artifacts/starlark-set-edge-contract.json`.

`BAZEL_RS_MODIFICATIONS.md` is a separately declared local notice for the modified
profiling CSV, whose machine-readable format cannot carry a source comment. It
is not an upstream NOTICE file. Rust patches carry inline modification notices.

## Verification and scope

The extraction inventory rejects absolute paths, parent traversal, duplicate
paths, links and special files before copying any member. Verification compares
every original member directly with its archive bytes. The portable verifier
checks declared current patches and reconstructs that original inventory;
unexpected files are rejected except separately declared additions.

Before/after `cargo metadata --locked --offline --format-version 1` receipts
are `artifacts/starlark-vendor-before-metadata.json` and
`artifacts/starlark-vendor-after-metadata.json`. Compare full resolution nodes,
dependency edges and enabled features after normalizing only the Starlark
registry-to-path package ID. All other package metadata and lockfile entries
must remain identical. The workspace must still contain only `bazel-rs`.

Use the repository-local pinned toolchain through `direnv exec .`. The parent
verification gate owns full tests, strict Clippy, formatting, analyzer, release
build, frozen-binary regression surfaces and final review. A dependency-source
transition can change compiled bytes; identical executable hashes are not a
semantic-equivalence requirement. Upstream files must not be reformatted to
make a broader formatting command pass. Record any path-dependency lint or
format-scope failure rather than silently changing the preserved source.

## Verified H0 baseline results (historical)

The baseline passes 952 Rust and 533 Python tests, formatting, strict Clippy,
release and locked/offline builds. Twelve native/reference regression suites pass
on the frozen candidate. Direct comparison with the preserved pre-vendor binary
matches cold/warm/source-edit outputs, cache behavior and invalid-root rejection.
The application checkout and required Git symlink remain unchanged; loading still
stops at missing `set`. No set enablement or performance improvement is claimed.

Analyzer status is **KNOWN_BASELINE_ERRORS**, not clean: vendoring exposes twelve
diagnostics within unchanged dependency files. An isolated, separately extracted
pristine archive with equivalent Cargo configuration reproduces the exact same
diagnostic multiset; a final current-tree scan matches it. All three scans report
zero root-source/test errors. This establishes reproducibility, not the cause of
the diagnostics. Raw scans, metadata equality and cleanup are retained in
`artifacts/h0-analyzer-baseline-comparison.json`.

The original trailing whitespace in `src/values/layout/heap/send.rs:56` is also
preserved byte-for-byte rather than rewriting upstream source. The non-vendor
staged whitespace check passes. Source and report bindings are recorded in
`artifacts/h0-vendor-verification-manifest.json`.

The [`starlark_doc_extract` increment](STARLARK_DOC_EXTRACT.md) adds one
read-only accessor to `src/eval/compiler/def.rs` (re-exported from
`src/eval.rs`): `starlark::eval::def_documentation(value)` returns a `def` or
`lambda`'s raw docstring, its Bazel location (the identifier span), its
parameter names as written (the parameter list names `*args`/`**kwargs`
generically) and the globals of its defining module in slot order
(StarlarkFunction.getDocumentation/getLocation/getModule). Nothing else
changes; both files' current hashes are updated in the same provenance.
