# A struct or provider instance is hashable when it is immutable
# (StarlarkInfo/StructImpl.isImmutable): frozen list and dict fields are
# fine (rules_cc's ExtraLinkTimeLibraryInfo `_key`), a list itself is not.
load(":lists.bzl", "D", "L")

P = provider(fields = ["l", "d"])

_d = {P(l = L, d = D): "provider"}

print("HASH", _d[P(l = L, d = D)], P(l = L, d = D) in {P(l = L, d = D): 1})

_s = {struct(l = L, d = D): "struct"}

print("STRUCT", _s[struct(d = D, l = L)])

def t(x):
    return x
