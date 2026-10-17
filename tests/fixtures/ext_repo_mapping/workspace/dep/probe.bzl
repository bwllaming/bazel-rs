# Source of a generated repository's defs.bzl: literal Label() calls, so each
# label resolves in the GENERATED repository's .bzl context (Label() uses the
# innermost .bzl's package context).
def probe_source(prefix, texts):
    source = 'def _p(text, label):\n'
    source += '    print("N45|%s|%%s|str=%%s|name=%%s|package=%%s" %% (text, str(label), label.name, label.package))\n' % prefix
    for text in texts:
        source += '_p(%r, Label(%r))\n' % (text, text)
    return source + "probe = 1\n"
