load(":root_export.bzl", "ROOT", "ROOT_DICT")
load(":root_helper.bzl", "copy_root", "local_root", "shared_root")
load("@shared//:export.bzl", "EXTERNAL", "EXTERNAL_SHARED", "EXTERNAL_DICT")
load("@shared//:helper.bzl", "copy_dep", "local_dep", "shared_dep")
load(":reader_a.bzl", "read_a")
load(":reader_b.bzl", "read_b")

def check(name, original, copied, canonical):
    if type(copied) != "Label" or copied != original or str(copied) != canonical:
        fail("COPY_CONTRACT", name, original, copied, canonical)
    values = {original: "original"}
    if values[copied] != "original":
        fail("DICT_LOOKUP", name)
    values[copied] = "copy"
    if len(values) != 1 or values[original] != "copy":
        fail("DICT_HASH_EQUALITY", name)
    if hash(str(original)) != hash(str(copied)):
        fail("STRING_HASH", name)
    print("COPY_OK", name, str(copied), copied.workspace_name, "dict=1", "string_hash_equal=True")

def run_probe():
    check("root-fresh", ROOT, Label(ROOT), "@@//:same")
    check("external-frozen-root-reader", EXTERNAL, Label(EXTERNAL), "@@dep+//:same")
    check("external-to-root-helper", EXTERNAL, copy_root(EXTERNAL), "@@dep+//:same")
    check("root-to-external-helper", ROOT, copy_dep(ROOT), "@@//:same")
    check("external-shared-to-root", EXTERNAL_SHARED, copy_root(EXTERNAL_SHARED), "@@other+//:same")
    check("root-shared-to-external", shared_root(), copy_dep(shared_root()), "@@dep+//:same")
    check("root-local-string", ROOT, local_root(), "@@//:same")
    check("external-local-string", EXTERNAL, local_dep(), "@@dep+//:same")
    check("external-shared-string", EXTERNAL_SHARED, shared_dep(), "@@other+//:same")
    check("nested-copy", EXTERNAL, Label(Label(EXTERNAL)), "@@dep+//:same")
    if ROOT_DICT[copy_dep(ROOT)] != "root-frozen-key" or EXTERNAL_DICT[copy_root(EXTERNAL)] != "external-frozen-key":
        fail("FROZEN_DICT")
    a, b = read_a(), read_b()
    if a != b or a[0] != EXTERNAL or a[1] != "external-frozen-key" or a[2] != "@@dep+//:same":
        fail("TWO_READERS", a, b)
    print("FROZEN_EXPORT_TWO_READERS_OK @@dep+//:same external-frozen-key")
    print("POSITIVE_COMPLETE")
    native.filegroup(name="ok")
