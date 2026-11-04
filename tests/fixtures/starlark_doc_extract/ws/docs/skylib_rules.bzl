"""Re-exports of bazel_skylib's rules."""

load("@bazel_skylib//rules:analysis_test.bzl", _analysis_test = "analysis_test")
load("@bazel_skylib//rules:build_test.bzl", _build_test = "build_test")
load("@bazel_skylib//rules:common_settings.bzl", _BuildSettingInfo = "BuildSettingInfo", _bool_flag = "bool_flag", _bool_setting = "bool_setting", _int_flag = "int_flag", _string_flag = "string_flag", _string_list_flag = "string_list_flag", _repeatable_string_flag = "repeatable_string_flag")
load("@bazel_skylib//rules:copy_directory.bzl", _copy_directory = "copy_directory")
load("@bazel_skylib//rules:copy_file.bzl", _copy_file = "copy_file")
load("@bazel_skylib//rules:diff_test.bzl", _diff_test = "diff_test")
load("@bazel_skylib//rules:expand_template.bzl", _expand_template = "expand_template")
load("@bazel_skylib//rules:native_binary.bzl", _native_binary = "native_binary", _native_test = "native_test")
load("@bazel_skylib//rules:run_binary.bzl", _run_binary = "run_binary")
load("@bazel_skylib//rules:select_file.bzl", _select_file = "select_file")
load("@bazel_skylib//rules:write_file.bzl", _write_file = "write_file")
load("@bazel_skylib//:bzl_library.bzl", _StarlarkLibraryInfo = "StarlarkLibraryInfo", _bzl_library = "bzl_library")

analysis_test = _analysis_test
build_test = _build_test
BuildSettingInfo = _BuildSettingInfo
bool_flag = _bool_flag
bool_setting = _bool_setting
int_flag = _int_flag
string_flag = _string_flag
string_list_flag = _string_list_flag
repeatable_string_flag = _repeatable_string_flag
copy_directory = _copy_directory
copy_file = _copy_file
diff_test = _diff_test
expand_template = _expand_template
native_binary = _native_binary
native_test = _native_test
run_binary = _run_binary
select_file = _select_file
write_file = _write_file
StarlarkLibraryInfo = _StarlarkLibraryInfo
bzl_library = _bzl_library
