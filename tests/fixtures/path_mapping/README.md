# Path mapping (`--experimental_output_paths=strip`)

`capture.py` runs genuine Bazel 9.3.1 (`--batch --ignore_all_rc_files`, a
private output user root, `env -i`) on `workspace/` and records
`reference_receipt.json`; `tests/path_mapping.rs` replays every case
natively (docs/PATH_MAPPING.md):

- `local_strategy`: a mapped spawn with `--spawn_strategy=local` fails with
  genuine's "requires sandboxing due to path mapping" message.
- `analysis`: `Target.actions` of mapped spawns (File arguments, map_each over
  `File.path`/`dirname`/`root.path`, a tree artifact, a parameter file, the exec
  configuration tool), of an unmapped one (no `supports-path-mapping`), of the
  collision fallback (two configurations' `g.txt`), of parameter-file writes,
  and of an analysis test transitioning `--modify_execution_info` (the
  ST- hashes of its target and exec configurations).
- `build_a`, `build_b`: the mapped spawn of two configurations (its argv,
  staged inputs at `bazel-out/cfg/...` and remote action digest from the JSON
  execution log; the outputs written at their own paths); the second is a
  disk cache hit of the first.
- `collisions`: equal contents run mapped, different contents unmapped.
- `param_writes`: `ctx.actions.write` of an Args with and without
  `supports-path-mapping`, also over a tree artifact.
- `aquery`: command lines and action keys (the `# Configuration:` checksums
  are the native ConfigIds, docs/AQUERY.md).

Recapture: `python3 capture.py <bazel-9.3.1> <scratch dir>`.
