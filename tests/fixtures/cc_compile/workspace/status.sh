#!/bin/sh
# The fixed workspace-status script of the N63 paired capture: deterministic
# stable keys (one overriding the built-in BUILD_USER) and the standard
# volatile source-control keys bazel_cc_build_info translates into
# volatile_file.h (BUILD_SCM_REVISION/BUILD_SCM_STATUS).
echo "STABLE_COMMIT deadbeefcafe"
echo "STABLE_SCM_STATUS clean"
echo "BUILD_USER stampuser"
echo "BUILD_EMBED_LABEL n63-fixture"
echo "BUILD_SCM_REVISION deadbeefcafe"
echo "BUILD_SCM_STATUS clean"
