"""A repository rule that finds the embedded JDK the way rules_jvm_external's
coursier does with `--repo_env=JAVA_HOME=../bazel_tools/jdk`
(private/rules/coursier.bzl _java_path: rctx.path(JAVA_HOME + "/bin/java"))
and runs it, `-version` and a coursier-like `java -jar`. It loads nothing
from @bazel_tools, so nothing but the layout of the repository directories
makes `../bazel_tools` exist."""

def _line(lines, key, value):
    lines.append("%s=%s" % (key, value))

def _probe_impl(rctx):
    lines = []
    here = str(rctx.path("."))
    parent = here[:here.rfind("/")]
    java_home = rctx.os.environ.get("JAVA_HOME")
    _line(lines, "java_home", java_home)
    java = rctx.path(java_home + "/bin/java")
    _line(lines, "java_path", str(java).replace(parent, "<EXTERNAL>"))
    _line(lines, "java_exists", java.exists)
    _line(lines, "jdk_release_exists", rctx.path("../bazel_tools/jdk/release").exists)
    _line(lines, "module_exists", rctx.path("../bazel_tools/MODULE.bazel").exists)
    version = rctx.execute([java, "-version"])
    _line(lines, "version_rc", version.return_code)
    _line(lines, "version_first", version.stderr.split("\n")[0])
    jar = rctx.execute([java, "-jar", rctx.path(Label("//:hello.jar")), "a", "b"])
    _line(lines, "jar_rc", jar.return_code)
    _line(lines, "jar_out", jar.stdout.strip())

    # The same through the shell, relative to the working directory, as a
    # wrapper script would (the java binary keeps its executable bit).
    sh = rctx.execute(["/bin/sh", "-c", "test -x ../bazel_tools/jdk/bin/java && ../bazel_tools/jdk/bin/java -XshowSettings:properties -version 2>&1 | grep -c 'java.home'"])
    _line(lines, "sh_rc", sh.return_code)
    _line(lines, "sh_out", sh.stdout.strip())
    rctx.file("result.txt", "\n".join(lines) + "\n")
    rctx.file("BUILD.bazel", """
genrule(
    name = "show",
    srcs = ["result.txt"],
    outs = ["show.txt"],
    cmd = "cp $< $@",
)
""")

probe = repository_rule(
    implementation = _probe_impl,
    environ = ["JAVA_HOME"],
)

def _jdk_like_impl(rctx):
    """rules_java's local_java_repository with JAVA_HOME set
    (toolchains/local_java_repository.bzl _determine_java_home and the
    readdir/symlink loop): the JDK's top-level entries linked into this
    repository, then `bin/java` run through those links."""
    java_home = rctx.path(rctx.os.environ["JAVA_HOME"])
    here = str(rctx.path("."))
    parent = here[:here.rfind("/")]
    lines = []
    _line(lines, "java_home", str(java_home).replace(parent, "<EXTERNAL>"))
    names = []
    for file in java_home.readdir():
        rctx.symlink(file, file.basename)
        names.append(file.basename)
    _line(lines, "entries", ",".join(sorted(names)))
    java = rctx.path("bin/java")
    _line(lines, "java_exists", java.exists)
    version = rctx.execute([java, "-version"])
    _line(lines, "version_rc", version.return_code)
    _line(lines, "version_first", version.stderr.split("\n")[0])
    rctx.file("result.txt", "\n".join(lines) + "\n")
    rctx.file("BUILD.bazel", 'exports_files(["result.txt"])\n')

jdk_like = repository_rule(
    implementation = _jdk_like_impl,
    environ = ["JAVA_HOME"],
)
