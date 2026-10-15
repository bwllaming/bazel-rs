# Provenance of the vendored test files

`vendor/` holds a **test-only** copy of the Starlark files that the probe in
`tools/build_defs/cc/probe.bzl` reaches (its full `load()` closure, computed
from `@rules_cc//cc/private:cc_common.bzl`, `@rules_cc//cc:cc_toolchain_config_lib.bzl`
and `@rules_cc//cc/private/toolchain_config:configure_features.bzl`). It is used
only by `tests/cc_features.rs` through `local_path_override` and is not part of
the bazel-rs product.

Source: the genuine Bazel module cache of this machine,
`/home/coder/.cache/bazel/_bazel_coder/c133fb6f7a443d300f42ed363b8adea5/external/`
(read only), modules `rules_cc` 0.2.17 (`rules_cc+`), `bazel_skylib` 1.9.0
(`bazel_skylib+`) and `platforms` 1.1.0 (`platforms`). The `.bzl` files and
`LICENSE` files are byte copies (Apache License 2.0; every copied `.bzl` keeps
its original Apache 2.0 header). Nothing else from those modules is included.

Deliberate modifications (all test-only):

- Each `MODULE.bazel` is a minimal replacement declaring only the module name,
  version, compatibility level and the `bazel_dep`s the copied files need
  (`rules_cc` -> `bazel_skylib`, `platforms`). The genuine files declare further
  dependencies (protobuf, bazel_features), extensions (`cc_configure`) and
  toolchain registrations that the probe never reaches. Original sha256:
  `rules_cc+/MODULE.bazel` 1849602c86cb60da8613d2de887f9566a6d354a6df6d7009f9d04a14402f9a84,
  `bazel_skylib+/MODULE.bazel` 72997b29dfd95c3fa0d0c48322d05590418edef451f8db8db5509c57875fb4b7,
  `platforms/MODULE.bazel` 1c0c09f5bdcf4b3f924720d2478a3711cb39f4977019ca5988685e5b7e18b3d2.
- Every package directory has an empty `BUILD.bazel` placeholder instead of the
  genuine BUILD file (only package existence matters for `.bzl` loads).
- `platforms` contributes only its `MODULE.bazel` replacement and `LICENSE`:
  rules_cc's `cc/common/semantics.bzl` resolves `Label("@platforms//os:windows")`
  at load time, which needs the repository mapping but no files.

This subset is a native-only fixture. Genuine Bazel 9.3 cannot build it:
its `bazel_tools` needs the complete modules (rules_cc's genuine MODULE.bazel
and BUILD files, platforms' host constraints such as
`@platforms//host:constraints.bzl`, bazel_features and package_metadata). The
paired genuine capture (`tests/fixtures/cc_features_reference_receipt.json`,
harness `artifacts/n25-capture.py`) therefore runs the same probe files on both
engines against the full genuine cached modules; the copied `.bzl` bytes here
are identical to those, so the probe evaluates the same rules_cc code.

| Fixture path | Cache path | sha256 |
| --- | --- | --- |
| `bazel_skylib/lib/paths.bzl` | `bazel_skylib+/lib/paths.bzl` | `96cce43871d8228126a12ceff771351f9030b1e9d029f2185853aa6541766a83` |
| `rules_cc/cc/action_names.bzl` | `rules_cc+/cc/action_names.bzl` | `e52d16474bd3ad3a0e0a4cd0cb1ad60b968ac5b0b2bcb0b1cffe85aedf80ed9d` |
| `rules_cc/cc/cc_toolchain_config_lib.bzl` | `rules_cc+/cc/cc_toolchain_config_lib.bzl` | `f8418490663f7e188fa060265b215e80b154a5d190b32ef75fcb6d0254017808` |
| `rules_cc/cc/common/cc_helper_internal.bzl` | `rules_cc+/cc/common/cc_helper_internal.bzl` | `793ab429f8e397df9c486f4c3c7b5c57fae81c8432ba6d08189d65d75676dae1` |
| `rules_cc/cc/common/semantics.bzl` | `rules_cc+/cc/common/semantics.bzl` | `029254fd58eb8b3bf32a0f772e479b991a51ce21a6f6cc8a5739aadbce3900da` |
| `rules_cc/cc/private/cc_common.bzl` | `rules_cc+/cc/private/cc_common.bzl` | `5e6ab737945b487759c9f039c77a066dc65bbe15cf590b566fe86029cc610762` |
| `rules_cc/cc/private/cc_info.bzl` | `rules_cc+/cc/private/cc_info.bzl` | `4424bb876c3f8234d7cfce20652e7ab1a7b2fc34cc2c637b1cb4313590d9f1bc` |
| `rules_cc/cc/private/cc_internal.bzl` | `rules_cc+/cc/private/cc_internal.bzl` | `8241ced58c265334ac3f0e063d492383f1ff7d223736dc2d6a5aa712165de6bb` |
| `rules_cc/cc/private/cc_launcher_info.bzl` | `rules_cc+/cc/private/cc_launcher_info.bzl` | `41da54762e854191c0217575d385b37cd9729380d7c78d3efbc19049177250dd` |
| `rules_cc/cc/private/cc_shared_library_hint_info.bzl` | `rules_cc+/cc/private/cc_shared_library_hint_info.bzl` | `7d067aad7862af26ee701dfa32c611d608fd606aaba06ca7dc232b6d7291d415` |
| `rules_cc/cc/private/compile/cc_compilation_helper.bzl` | `rules_cc+/cc/private/compile/cc_compilation_helper.bzl` | `2c484cade81f0d70efd203612b5492e5578871b2b3e9be7987de42de9c57863f` |
| `rules_cc/cc/private/compile/cc_compilation_outputs.bzl` | `rules_cc+/cc/private/compile/cc_compilation_outputs.bzl` | `294e3da16da4444122e7dee058ec1e06b30cec93d64a32f217cf9e1e3e4bfb44` |
| `rules_cc/cc/private/compile/compile.bzl` | `rules_cc+/cc/private/compile/compile.bzl` | `bec506ffc3be08fffc4842b9daac498773534db9916121648a5527fac84cabea` |
| `rules_cc/cc/private/compile/compile_action_templates.bzl` | `rules_cc+/cc/private/compile/compile_action_templates.bzl` | `10a43c512a85458f45a0223a7ddc7c1b56f8072872b765b1744d336ff91ec794` |
| `rules_cc/cc/private/compile/compile_build_variables.bzl` | `rules_cc+/cc/private/compile/compile_build_variables.bzl` | `463ea66c2423cab80153ccbb25193516e00e887d24bd4ba6d7b485a19c8d8b54` |
| `rules_cc/cc/private/compile/linkstamp_compile.bzl` | `rules_cc+/cc/private/compile/linkstamp_compile.bzl` | `6f5ceb39f1b6c26b65073867f3435ec01093775edf6129d2b9421bca4c7a70bb` |
| `rules_cc/cc/private/compile/lto_compilation_context.bzl` | `rules_cc+/cc/private/compile/lto_compilation_context.bzl` | `a17435cd56fa165c71081e99f9af73407f7b4cc1dc086e53771dcf74df81b3f4` |
| `rules_cc/cc/private/link/cc_linking_helper.bzl` | `rules_cc+/cc/private/link/cc_linking_helper.bzl` | `c45dd243835bd70803a7bb2e0a11167c9ea5ac912b3f02d415841879873b2a03` |
| `rules_cc/cc/private/link/collect_solib_dirs.bzl` | `rules_cc+/cc/private/link/collect_solib_dirs.bzl` | `f25b0f978bce3a3cf810b36c6897a85adefce7036ec68ba53613352afa218125` |
| `rules_cc/cc/private/link/cpp_link_action.bzl` | `rules_cc+/cc/private/link/cpp_link_action.bzl` | `0cbe9d6b0ce0f6bea5abe1d9783b79435f495ba93bdaf402ad9539513a82223f` |
| `rules_cc/cc/private/link/create_extra_link_time_library.bzl` | `rules_cc+/cc/private/link/create_extra_link_time_library.bzl` | `522312ac48567566725f0768a6961fcaa78577fa24ac8007d5b1b8ca19698e82` |
| `rules_cc/cc/private/link/create_libraries_to_link_values.bzl` | `rules_cc+/cc/private/link/create_libraries_to_link_values.bzl` | `7d8df512d6b0df2178a2ca9cd30cb36d1a22c96877dd8e69f49bd3cf739a3764` |
| `rules_cc/cc/private/link/create_library_to_link.bzl` | `rules_cc+/cc/private/link/create_library_to_link.bzl` | `5f57423312f24392f106aeb5959485c4f30c54ee2d8e926a45934de51a2455d1` |
| `rules_cc/cc/private/link/create_linker_input.bzl` | `rules_cc+/cc/private/link/create_linker_input.bzl` | `e4e8a7fc9d7be8edd40a2b95e72a96710c05d5bbd610b2c1cc2f274e3672cbd1` |
| `rules_cc/cc/private/link/create_linking_context_from_compilation_outputs.bzl` | `rules_cc+/cc/private/link/create_linking_context_from_compilation_outputs.bzl` | `664a461564abd348111d791aa03da0207fe158620d276b6da1936f8abb23be59` |
| `rules_cc/cc/private/link/create_linkstamp.bzl` | `rules_cc+/cc/private/link/create_linkstamp.bzl` | `8d5fc394e31c5f0eb8a84f5020f35e71f90cdbf89591e44d1c0da8a8899e6000` |
| `rules_cc/cc/private/link/finalize_link_action.bzl` | `rules_cc+/cc/private/link/finalize_link_action.bzl` | `adc6ea3b355d0c5e5fbf1b1e9eaa7d7dd7c0c095234a0cff7fdb4fc72eb167c9` |
| `rules_cc/cc/private/link/link.bzl` | `rules_cc+/cc/private/link/link.bzl` | `666e819dee4777d0c3d8624e18588a905046532a6668d89d5744419cbee4a0e2` |
| `rules_cc/cc/private/link/link_build_variables.bzl` | `rules_cc+/cc/private/link/link_build_variables.bzl` | `bdf030361c5a199f6c0fd1bbe5e3b1ce68d041141626a6b0242639b13eab33f0` |
| `rules_cc/cc/private/link/lto_backends.bzl` | `rules_cc+/cc/private/link/lto_backends.bzl` | `078bfb686e85b584745fcea2d9e5535938f9afc1a0066f80cc88aceb699f4226` |
| `rules_cc/cc/private/link/lto_indexing_action.bzl` | `rules_cc+/cc/private/link/lto_indexing_action.bzl` | `03cb57e972bb7503d665ca56340a34fff3e6289f9c7a168ca87a427e57c66863` |
| `rules_cc/cc/private/link/target_types.bzl` | `rules_cc+/cc/private/link/target_types.bzl` | `12110c7dce405cd2ba4253d694502f08cc97a95bd0004444054ae8aa689da8fd` |
| `rules_cc/cc/private/paths.bzl` | `rules_cc+/cc/private/paths.bzl` | `c982ac685f0bfbd32602d82d1c37f3bf50a2714ca6a13bfd3c08d4e5cc8b8872` |
| `rules_cc/cc/private/rules_impl/cc_toolchain_info.bzl` | `rules_cc+/cc/private/rules_impl/cc_toolchain_info.bzl` | `f19589572147b7dc8f1b16ab96791b7651923c36821aed70868a74bbfce963f5` |
| `rules_cc/cc/private/rules_impl/native_cc_common.bzl` | `rules_cc+/cc/private/rules_impl/native_cc_common.bzl` | `d8e5fedab99534bd1a926dd780e2bfdc66e9a8e70cd29561a591969123084e46` |
| `rules_cc/cc/private/toolchain_config/cc_toolchain_config_info.bzl` | `rules_cc+/cc/private/toolchain_config/cc_toolchain_config_info.bzl` | `8c522773214e202b426ae43589f59a8bdbf3af19d2e595ba8ec7ac125fef5d39` |
| `rules_cc/cc/private/toolchain_config/configure_features.bzl` | `rules_cc+/cc/private/toolchain_config/configure_features.bzl` | `d950aa9acda68b999c452178f8ccf49860eac910a8c28551c547c3725198b977` |
| `rules_cc/cc/private/toolchain_config/legacy_features.bzl` | `rules_cc+/cc/private/toolchain_config/legacy_features.bzl` | `9a6cafe57d4f8564b8eea39a41e66e372471f4d3dcb2cb0e52c970d45f7695dd` |
| `rules_cc/LICENSE` | `rules_cc+/LICENSE` | `58d1e17ffe5109a7ae296caafcadfdbe6a7d176f0bc4ab01e12a689b0499d8bd` |
| `bazel_skylib/LICENSE` | `bazel_skylib+/LICENSE` | `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30` |
| `platforms/LICENSE` | `platforms/LICENSE` | `c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4` |
