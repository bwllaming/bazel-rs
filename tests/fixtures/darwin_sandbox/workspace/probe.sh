#!/bin/bash
# What a sandboxed spawn may do (scripts/host_parity.py case darwin_sandbox):
#   probe.sh OUT SOURCE_INPUT GENERATED_INPUT
# PROBE_WS and PROBE_EXECROOT (--action_env): the workspace and the
# execution root outside the sandbox; PROBE_NETWORK=0 skips the remote
# connection (tests/darwin_sandbox.rs, hermetic).
out=$1
try() {
  if (eval "$2") > /dev/null 2>&1; then echo "$1=allowed"; else echo "$1=denied"; fi
}
{
  try read_workspace "cat '$PROBE_WS/MODULE.bazel'"
  try read_zoneinfo "cat /usr/share/zoneinfo/UTC"
  try write_workspace "echo x > '$PROBE_WS/written_by_action'"
  try write_execroot "echo x > '$PROBE_EXECROOT/written_by_action'"
  try write_spawn_root "echo x > written_by_action"
  try write_tmp "echo x > /tmp/darwin_sandbox_probe.$$ && rm /tmp/darwin_sandbox_probe.$$"
  try write_private_var_tmp "echo x > /private/var/tmp/darwin_sandbox_probe.$$ && rm /private/var/tmp/darwin_sandbox_probe.$$"
  try write_TMPDIR "echo x > '${TMPDIR:-/nonexistent}/darwin_sandbox_probe.$$'"
  try write_home "echo x > ~/darwin_sandbox_probe.$$ && rm ~/darwin_sandbox_probe.$$"
  try write_home_library_caches "mkdir -p ~/Library/Caches && echo x > ~/Library/Caches/darwin_sandbox_probe.$$ && rm ~/Library/Caches/darwin_sandbox_probe.$$"
  try mkdir_users_shared "mkdir /Users/Shared/darwin_sandbox_probe.$$ && rmdir /Users/Shared/darwin_sandbox_probe.$$"
  if [ "${PROBE_NETWORK:-1}" = 0 ]; then
    echo network_remote=skipped
  else
    try network_remote "/usr/bin/curl -sS -m 20 -o /dev/null https://bcr.bazel.build/"
  fi
  try exec_ps "/bin/ps -p $$"
  echo "cf_user_text_encoding=${__CF_USER_TEXT_ENCODING+set}"
  echo "source_input_symlink=$([ -L "$2" ] && echo yes || echo no)"
  echo "generated_input_symlink=$([ -L "$3" ] && echo yes || echo no)"
  echo "darwin_user_temp_dir=${DARWIN_USER_TEMP_DIR+set}"
} > "$out"
