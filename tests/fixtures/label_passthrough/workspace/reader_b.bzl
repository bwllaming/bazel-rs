load("@shared//:export.bzl", "EXTERNAL", "EXTERNAL_DICT")
def read_b():
    copy = Label(EXTERNAL)
    return [copy, EXTERNAL_DICT[copy], str(copy)]
