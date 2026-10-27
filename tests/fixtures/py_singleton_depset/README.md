# py_internal.is_singleton_depset (paired genuine capture)

`workspace/` built with genuine Bazel 9.3.0
(`bazel-9.3.0-linux-x86_64 --ignore_all_rc_files build //tools/build_defs/python:<t>`):
`probe` writes `genuine_probe.txt` (byte-identical natively); `wrong_type` and
`named` fail analysis with

    Error in is_singleton_depset: in call to is_singleton_depset(), parameter 'value' got value of type 'list', want 'depset'
    Error in is_singleton_depset: is_singleton_depset() got named argument for positional-only parameter 'value'

(tests/py_singleton_depset.rs). Motivation: rules_python on ~/code
(`build --nobuild //src/python:bworker_env`) stopped at
`py_internal.is_singleton_depset is not implemented in the native profile`.
