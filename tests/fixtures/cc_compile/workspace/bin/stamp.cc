// A linkstamp source: compiled with the build-info headers force-included
// on the command line when the linking binary stamps (rules_cc
// linkstamp_compile.bzl adds them as -include inputs, the historic
// CppLinkstampCompile behavior), embedding the workspace status in the
// binary (tools/buildstamp).
#include <stdio.h>

int stamp_marker(void)
{
    printf("%s %s %ld\n", BUILD_SCM_REVISION, BUILD_EMBED_LABEL, (long) BUILD_TIMESTAMP);
    return 3;
}
