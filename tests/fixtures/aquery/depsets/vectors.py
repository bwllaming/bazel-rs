import zlib, hashlib, sys, os
def lcg(n, seed, mod):
    x = seed; out = bytearray()
    for _ in range(n):
        x = (x * 1103515245 + 12345) & 0x7fffffff
        out.append((x >> 16) % mod)
    return bytes(out)
def words(n, seed):
    ws = [b"alpha", b"beta", b"gamma", b"delta", b"epsilon", b"zeta", b"eta", b"theta", b"\n", b" ", b"iota", b"kappa_lambda_mu"]
    x = seed; out = bytearray()
    while len(out) < n:
        x = (x * 1103515245 + 12345) & 0x7fffffff
        out += ws[(x >> 16) % len(ws)]
    return bytes(out[:n])
def samples():
    yield "c257", b"c" * 257
    yield "lines", ("line %d\n" * 20 + "\n".join("entry-%d" % i for i in range(300))).encode()
    yield "random", lcg(100000, 7, 256)
    yield "dna", lcg(300000, 11, 4)
    yield "words", words(500000, 3)
    yield "sparse", lcg(200000, 5, 2)
def deflate(d):
    c = zlib.compressobj(6, zlib.DEFLATED, -15, 8); return c.compress(d) + c.flush()
if __name__ == "__main__":
    for name, d in samples():
        print(name, hashlib.sha256(deflate(d)).hexdigest())
    if len(sys.argv) > 1:
        import random
        os.makedirs(sys.argv[1], exist_ok=True)
        r = random.Random(42)
        for i in range(int(sys.argv[2])):
            kind = r.randrange(5); n = r.choice([257, 300, 1000, 5000, 33000, 65536, 70000, 140000, 400000, r.randrange(257, 600000)])
            if kind == 0: d = bytes(r.randrange(256) for _ in range(n))
            elif kind == 1: d = bytes(r.choice(b"ab") for _ in range(n))
            elif kind == 2: d = words(n, r.randrange(1 << 30))
            elif kind == 3: d = lcg(n, r.randrange(1 << 30), r.choice([3, 16, 64]))
            else:
                parts = []; tot = 0
                while tot < n:
                    k = r.randrange(4)
                    if k == 0: p = bytes(r.randrange(256) for _ in range(r.randrange(1, 2000)))
                    elif k == 1: p = bytes([r.randrange(256)]) * r.randrange(1, 5000)
                    elif k == 2 and parts: q = r.choice(parts); p = q[: r.randrange(1, len(q) + 1)]
                    else: p = words(r.randrange(1, 3000), r.randrange(1000))
                    parts.append(p); tot += len(p)
                d = b"".join(parts)[:n]
            open(os.path.join(sys.argv[1], "%03d.in" % i), "wb").write(d)
            open(os.path.join(sys.argv[1], "%03d.deflate" % i), "wb").write(deflate(d))
