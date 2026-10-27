/*
 * String functions for the static musl builds of bazel-rs (build.rs compiles
 * this file only when the target's env is "musl"; docs/RELEASING.md).
 *
 * musl's memcmp and bcmp compare one byte per iteration and its x86_64 memcpy
 * and memmove are `rep movsq` loops with a large start-up cost for the short,
 * variable-length copies Rust code does all the time (string and label
 * comparisons, Vec/String growth, hashing keys). Measured on ~/code
 * (`build //src/beyondwork.ai/tools/oauth2token/...` with the analysis
 * snapshot off): the musl binary took 4.9-5.1 s against glibc's 3.7-4.0 s;
 * with these functions 3.1-3.4 s. They are linked before libc.a (whole
 * archive), so libc's own definitions are never pulled in.
 *
 * Only the semantics the C standard guarantees are implemented: memcmp's sign,
 * bcmp's zero / non-zero, memcpy for non-overlapping and memmove for
 * overlapping buffers. Every copy below 33 bytes loads all source bytes
 * before it stores any, so it is overlap-safe. tests/libc_string.rs checks
 * them against byte loops through Rust's slice operations. Compiled with
 * -ffreestanding -fno-builtin and the no_builtin attribute so the compiler
 * never turns a loop back into a call to the function being defined.
 */
#include <stddef.h>
#include <stdint.h>

#if defined(__has_attribute)
#if __has_attribute(no_builtin)
#define NO_BUILTIN __attribute__((no_builtin))
#endif
#endif
#ifndef NO_BUILTIN
#if defined(__GNUC__) && !defined(__clang__)
#define NO_BUILTIN __attribute__((optimize("no-tree-loop-distribute-patterns")))
#else
#define NO_BUILTIN
#endif
#endif

typedef struct { uint64_t a, b; } v16;

static inline uint64_t ld8(const unsigned char *p) { uint64_t v; __builtin_memcpy(&v, p, 8); return v; }
static inline uint32_t ld4(const unsigned char *p) { uint32_t v; __builtin_memcpy(&v, p, 4); return v; }
static inline uint16_t ld2(const unsigned char *p) { uint16_t v; __builtin_memcpy(&v, p, 2); return v; }
static inline v16 ld16(const unsigned char *p) { v16 v; __builtin_memcpy(&v, p, 16); return v; }
static inline void st8(unsigned char *p, uint64_t v) { __builtin_memcpy(p, &v, 8); }
static inline void st4(unsigned char *p, uint32_t v) { __builtin_memcpy(p, &v, 4); }
static inline void st2(unsigned char *p, uint16_t v) { __builtin_memcpy(p, &v, 2); }
static inline void st16(unsigned char *p, v16 v) { __builtin_memcpy(p, &v, 16); }

static inline int cmp8(uint64_t x, uint64_t y) {
    x = __builtin_bswap64(x);
    y = __builtin_bswap64(y);
    return x < y ? -1 : 1;
}

NO_BUILTIN int memcmp(const void *va, const void *vb, size_t n) {
    const unsigned char *l = va, *r = vb;
    if (n >= 8) {
        while (n > 8) {
            uint64_t x = ld8(l), y = ld8(r);
            if (x != y) return cmp8(x, y);
            l += 8; r += 8; n -= 8;
        }
        /* 1..8 bytes left: compare the last 8 (overlapping what was equal). */
        uint64_t x = ld8(l + n - 8), y = ld8(r + n - 8);
        return x == y ? 0 : cmp8(x, y);
    }
    if (n >= 4) {
        uint64_t x = ((uint64_t)__builtin_bswap32(ld4(l)) << 32) | __builtin_bswap32(ld4(l + n - 4));
        uint64_t y = ((uint64_t)__builtin_bswap32(ld4(r)) << 32) | __builtin_bswap32(ld4(r + n - 4));
        return x == y ? 0 : (x < y ? -1 : 1);
    }
    for (; n; n--, l++, r++)
        if (*l != *r) return *l - *r;
    return 0;
}

NO_BUILTIN int bcmp(const void *va, const void *vb, size_t n) {
    const unsigned char *l = va, *r = vb;
    if (n >= 16) {
        uint64_t acc;
        while (n > 16) {
            v16 x = ld16(l), y = ld16(r);
            if ((x.a ^ y.a) | (x.b ^ y.b)) return 1;
            l += 16; r += 16; n -= 16;
        }
        v16 x = ld16(l + n - 16), y = ld16(r + n - 16);
        acc = (x.a ^ y.a) | (x.b ^ y.b);
        return acc != 0;
    }
    if (n >= 8) return ((ld8(l) ^ ld8(r)) | (ld8(l + n - 8) ^ ld8(r + n - 8))) != 0;
    if (n >= 4) return ((ld4(l) ^ ld4(r)) | (ld4(l + n - 4) ^ ld4(r + n - 4))) != 0;
    if (n >= 2) return ((ld2(l) ^ ld2(r)) | (ld2(l + n - 2) ^ ld2(r + n - 2))) != 0;
    return n ? *l != *r : 0;
}

/* Copies n <= 32 bytes; all loads happen before any store, so overlap is fine. */
static inline void copy_small(unsigned char *d, const unsigned char *s, size_t n) {
    if (n >= 16) {
        v16 a = ld16(s), b = ld16(s + n - 16);
        st16(d, a); st16(d + n - 16, b);
    } else if (n >= 8) {
        uint64_t a = ld8(s), b = ld8(s + n - 8);
        st8(d, a); st8(d + n - 8, b);
    } else if (n >= 4) {
        uint32_t a = ld4(s), b = ld4(s + n - 4);
        st4(d, a); st4(d + n - 4, b);
    } else if (n >= 2) {
        uint16_t a = ld2(s), b = ld2(s + n - 2);
        st2(d, a); st2(d + n - 2, b);
    } else if (n) {
        *d = *s;
    }
}

NO_BUILTIN void *memcpy(void *restrict vd, const void *restrict vs, size_t n) {
    unsigned char *d = vd;
    const unsigned char *s = vs;
    if (n <= 32) {
        copy_small(d, s, n);
        return vd;
    }
    if (n <= 512) {
        v16 t0 = ld16(s + n - 32), t1 = ld16(s + n - 16);
        size_t i = 0;
        for (; i + 32 < n; i += 32) {
            v16 a = ld16(s + i), b = ld16(s + i + 16);
            st16(d + i, a); st16(d + i + 16, b);
        }
        st16(d + n - 32, t0); st16(d + n - 16, t1);
        return vd;
    }
#if defined(__x86_64__)
    /* Large copies: `rep movsb` (fast-strings microcode on every x86_64 CPU
     * of the last decade). */
    __asm__ volatile("rep movsb" : "+D"(d), "+S"(s), "+c"(n) : : "memory");
#else
    v16 t0 = ld16(s + n - 32), t1 = ld16(s + n - 16);
    for (size_t i = 0; i + 32 < n; i += 32) {
        v16 a = ld16(s + i), b = ld16(s + i + 16);
        st16(d + i, a); st16(d + i + 16, b);
    }
    st16(d + n - 32, t0); st16(d + n - 16, t1);
#endif
    return vd;
}

NO_BUILTIN void *memmove(void *vd, const void *vs, size_t n) {
    unsigned char *d = vd;
    const unsigned char *s = vs;
    if (n <= 32) {
        copy_small(d, s, n);
        return vd;
    }
    if ((uintptr_t)d - (uintptr_t)s >= n) {
        /* d before s, or no overlap: forward, the last 32 bytes loaded first. */
        v16 t0 = ld16(s + n - 32), t1 = ld16(s + n - 16);
        size_t i = 0;
        for (; i + 32 < n; i += 32) {
            v16 a = ld16(s + i), b = ld16(s + i + 16);
            st16(d + i, a); st16(d + i + 16, b);
        }
        st16(d + n - 32, t0); st16(d + n - 16, t1);
        return vd;
    }
    /* d inside (s, s+n): backward, the first 32 bytes loaded first. */
    v16 h0 = ld16(s), h1 = ld16(s + 16);
    size_t i = n;
    for (; i > 32; i -= 32) {
        v16 a = ld16(s + i - 32), b = ld16(s + i - 16);
        st16(d + i - 32, a); st16(d + i - 16, b);
    }
    st16(d, h0); st16(d + 16, h1);
    return vd;
}
