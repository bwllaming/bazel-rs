#!/bin/bash
# What a sandboxed spawn may do (scripts/host_parity.py case darwin_sandbox):
#   probe.sh OUT SOURCE_INPUT GENERATED_INPUT
out=$1
src_dir=$(dirname "$(readlink "$2")")
gen_dir=$(dirname "$(readlink "$3")")
try() {
  if (eval "$2") > /dev/null 2>&1; then echo "$1=allowed"; else echo "$1=denied"; fi
}
{
  try read_workspace "cat '$src_dir/MODULE.bazel'"
  try write_workspace "echo x > '$src_dir/written_by_action'"
  try write_execroot "echo x > '$gen_dir/written_by_action'"
  try write_spawn_root "echo x > written_by_action"
  try write_tmp "echo x > /tmp/darwin_sandbox_probe.$$ && rm /tmp/darwin_sandbox_probe.$$"
  try write_private_var_tmp "echo x > /private/var/tmp/darwin_sandbox_probe.$$ && rm /private/var/tmp/darwin_sandbox_probe.$$"
  try write_TMPDIR "echo x > '${TMPDIR:-/nonexistent}/darwin_sandbox_probe.$$'"
  try write_home "echo x > ~/darwin_sandbox_probe.$$ && rm ~/darwin_sandbox_probe.$$"
  try write_home_library_caches "mkdir -p ~/Library/Caches && echo x > ~/Library/Caches/darwin_sandbox_probe.$$ && rm ~/Library/Caches/darwin_sandbox_probe.$$"
  try mkdir_users_shared "mkdir /Users/Shared/darwin_sandbox_probe.$$ && rmdir /Users/Shared/darwin_sandbox_probe.$$"
  try network_remote "/usr/bin/curl -sS -m 20 -o /dev/null https://bcr.bazel.build/"
  try exec_ps "/bin/ps -p $$"
} > "$out"
