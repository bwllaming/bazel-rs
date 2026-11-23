# tests/fixtures/corpus_rulescc

Hand-written workspace (`workspace/`, OSS corpus B32): Starlark transitions
writing and reading the FDO, Propeller, memprof, custom malloc, libc and
ThinLTO CppOptions, a rule whose late-bound `configuration_field("cpp", ...)`
attributes print their configured labels into an output file, `select()` on
the options and the exec transition of `--grte_top`. `reference_receipt.json`
is written by `capture.py` beside it with genuine Bazel 9.3.1
(`/home/coder/bazel/.direnv/reference/bazel-9.3.1-linux-x86_64`, `--batch
--ignore_all_rc_files`, one fresh copy per case); tests/corpus_rulescc.rs
replays every case natively (exit code, stderr lines, output paths with
their ST- hashes and contents).
