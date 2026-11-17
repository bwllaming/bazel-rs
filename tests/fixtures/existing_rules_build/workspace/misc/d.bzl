def m():
    r = native.existing_rules()
    print("DICT2 %r" % dict(r))
    print("SORT %r" % sorted(r))
    print("EQ %s %s" % (r == r, native.existing_rule("g") == native.existing_rule("g")))
    print("JSON %s" % json.encode(native.existing_rule("g")["tags"]))
    print("TUPLE %r" % (native.existing_rule("g")["tags"] + ("a",)))
    r.clear()
