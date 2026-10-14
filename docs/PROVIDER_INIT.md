# Provider initializers

`provider(init=...)` follows Bazel 9.3 (`e89755...`):

- `StarlarkRuleClassFunctions.provider` returns the bare provider when `init`
  is `None`. Otherwise it returns `(provider, raw_constructor)`.
- `StarlarkProvider.ArgumentProcessorWithInit` passes the caller's positional
  and keyword arguments to `init` unchanged.
- `StarlarkProvider.RawArgumentProcessor` and the `StarlarkInfoWithSchema` /
  `StarlarkInfoNoSchema` factories implement the default constructor.

## Definition

```starlark
def _init(*, metadata, purl = None):
    if not purl.startswith("pkg:"):
        fail("bad purl")
    return {"metadata": metadata, "purl": purl, "files": depset([metadata])}

PackageMetadataInfo, _create = provider(
    doc = "...",
    fields = {"metadata": "...", "purl": "...", "files": "..."},
    init = _init,
)
```

- `init` can be any callable value: a `def`, a lambda, a builtin, or another
  provider. Otherwise the call fails with
  `in call to provider(), parameter 'init' got value of type 'T', want 'callable or NoneType'`.
  As in Bazel's parameter binding, this error comes before element-level
  `fields` errors. A `fields` value with the wrong container type is still
  reported first.
- A top-level tuple or list assignment exports each bound name, as Bazel's
  post-assign hook does. The provider gets the name of its own binding,
  `PackageMetadataInfo` in the example above. The raw constructor is not
  exportable, and its name always comes from the provider's export.

## Calls

`P(*args, **kwargs)` calls `init(*args, **kwargs)`. Errors from the call
propagate unchanged, including signature mismatches and `fail()`. The result is
then checked:

- If it is not a dict, the call fails with
  `got T for 'return value of provider init()', want dict`.
- If the first non-string key has type `K` and its value has type `V`, the call
  fails with
  `got dict<K, V> for 'return value of provider init()', want dict<string, unknown>`.
- Otherwise the provider is instantiated as if called with `c(**d)`. With a
  schema, unknown names produce
  `got unexpected field(s) 'a', 'b' in call to instantiate provider P`. Names
  are listed in insertion order.

The raw constructor `r(**kwargs)` skips `init`:

- Any positional argument fails with
  `<raw constructor for P>: unexpected positional arguments`.
- A provider without `init` uses `P: unexpected positional arguments`.
- Keyword arguments are checked against the same schema.

Instances from either path have the same provider identity and instance type
(`struct`). They compare equal when their fields are equal, and analysis finds
both through `target[P]` and `P in target`.

## Raw constructor value

Bazel's `RawConstructor` has no `@StarlarkBuiltin` annotation and does not
override `repr`. This implementation copies that behaviour:

- `type(r) == "RawConstructor"`.
- `repr(r)` and `str(r)` are
  `<unknown object com.google.devtools.build.lib.packages.StarlarkProvider$RawConstructor>`.
- Equality is by identity and the value is unhashable.
- It is not a provider key. Using it in `providers=[...]`, `in` or `[]` fails.

## Remaining differences

- Native construction still requires an exported provider. This applies to the
  provider, to the raw constructor, and to calls with or without `init`. Bazel
  would build instances of an unexported provider named `<no name>`.
- Messages for `init` signature mismatches come from the Starlark interpreter,
  e.g. a positional argument to a keyword-only `init` gives ``Missing named-only
  parameter `metadata` ...`` where Bazel says `_init() does not accept positional
  arguments, but got 1`.
- Tracebacks show two `<builtin>` frames (`in <provider ...%Name>` and `in None`)
  where Bazel shows one frame `in Name` at the `provider(` call line.
- `repr(P)`/`str(P)` of the provider itself is unchanged: `<provider label%Name>`
  (Bazel: `<provider>`).
- `repr`/`str` of a provider instance print `Name(field = <File x>, ...)`; Bazel
  prints `struct(field = <source file x>, ...)` (predates init support).
- Hashing a raw constructor fails with ``Value of type `RawConstructor` is not
  hashable`` (Bazel: `unhashable type: 'RawConstructor'`); using it in
  `providers=[...]` fails with `required providers must be exported Provider
  constructors, got RawConstructor` (Bazel: `at index 0 of providers, got element
  of type RawConstructor, want sequence`). Both reject.

These were labelled by a genuine Bazel 9.3 paired capture of 16 scenarios
(`tests/fixtures/provider_init/reference_receipt.json`): the successful build is
byte-identical, every other test-mirrored scenario is exact, and the differences
above are the only ones observed.
