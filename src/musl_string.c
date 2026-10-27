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
 * them against byte loops through Rust's slice operations. strerror_r (end
 * of the file) gives glibc's error texts for Bazel-parity messages. Compiled with
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

/*
 * strerror_r with glibc's messages. Rust's io::Error text comes from
 * strerror_r, and bazel-rs repeats Bazel's error texts, which embed glibc's
 * strerror (Bazel's JNI, e.g. "[unix_jni.cc:382] <path> (Too many levels of
 * symbolic links)"); musl words several differently ("Symbolic link loop",
 * "Filename too long", "Resource busy", ...). The table is glibc 2.39's
 * (Linux errno numbers, identical on x86_64 and aarch64); other numbers read
 * "Unknown error N" as in glibc. musl's strerror_r object also defines
 * __xpg_strerror_r, so both are defined here to keep it out of the link.
 */
static const char *const glibc_errors[] = {
    [0] = "Success", /* 0 */
    [1] = "Operation not permitted", /* EPERM */
    [2] = "No such file or directory", /* ENOENT */
    [3] = "No such process", /* ESRCH */
    [4] = "Interrupted system call", /* EINTR */
    [5] = "Input/output error", /* EIO */
    [6] = "No such device or address", /* ENXIO */
    [7] = "Argument list too long", /* E2BIG */
    [8] = "Exec format error", /* ENOEXEC */
    [9] = "Bad file descriptor", /* EBADF */
    [10] = "No child processes", /* ECHILD */
    [11] = "Resource temporarily unavailable", /* EAGAIN */
    [12] = "Cannot allocate memory", /* ENOMEM */
    [13] = "Permission denied", /* EACCES */
    [14] = "Bad address", /* EFAULT */
    [15] = "Block device required", /* ENOTBLK */
    [16] = "Device or resource busy", /* EBUSY */
    [17] = "File exists", /* EEXIST */
    [18] = "Invalid cross-device link", /* EXDEV */
    [19] = "No such device", /* ENODEV */
    [20] = "Not a directory", /* ENOTDIR */
    [21] = "Is a directory", /* EISDIR */
    [22] = "Invalid argument", /* EINVAL */
    [23] = "Too many open files in system", /* ENFILE */
    [24] = "Too many open files", /* EMFILE */
    [25] = "Inappropriate ioctl for device", /* ENOTTY */
    [26] = "Text file busy", /* ETXTBSY */
    [27] = "File too large", /* EFBIG */
    [28] = "No space left on device", /* ENOSPC */
    [29] = "Illegal seek", /* ESPIPE */
    [30] = "Read-only file system", /* EROFS */
    [31] = "Too many links", /* EMLINK */
    [32] = "Broken pipe", /* EPIPE */
    [33] = "Numerical argument out of domain", /* EDOM */
    [34] = "Numerical result out of range", /* ERANGE */
    [35] = "Resource deadlock avoided", /* EDEADLOCK */
    [36] = "File name too long", /* ENAMETOOLONG */
    [37] = "No locks available", /* ENOLCK */
    [38] = "Function not implemented", /* ENOSYS */
    [39] = "Directory not empty", /* ENOTEMPTY */
    [40] = "Too many levels of symbolic links", /* ELOOP */
    [42] = "No message of desired type", /* ENOMSG */
    [43] = "Identifier removed", /* EIDRM */
    [44] = "Channel number out of range", /* ECHRNG */
    [45] = "Level 2 not synchronized", /* EL2NSYNC */
    [46] = "Level 3 halted", /* EL3HLT */
    [47] = "Level 3 reset", /* EL3RST */
    [48] = "Link number out of range", /* ELNRNG */
    [49] = "Protocol driver not attached", /* EUNATCH */
    [50] = "No CSI structure available", /* ENOCSI */
    [51] = "Level 2 halted", /* EL2HLT */
    [52] = "Invalid exchange", /* EBADE */
    [53] = "Invalid request descriptor", /* EBADR */
    [54] = "Exchange full", /* EXFULL */
    [55] = "No anode", /* ENOANO */
    [56] = "Invalid request code", /* EBADRQC */
    [57] = "Invalid slot", /* EBADSLT */
    [59] = "Bad font file format", /* EBFONT */
    [60] = "Device not a stream", /* ENOSTR */
    [61] = "No data available", /* ENODATA */
    [62] = "Timer expired", /* ETIME */
    [63] = "Out of streams resources", /* ENOSR */
    [64] = "Machine is not on the network", /* ENONET */
    [65] = "Package not installed", /* ENOPKG */
    [66] = "Object is remote", /* EREMOTE */
    [67] = "Link has been severed", /* ENOLINK */
    [68] = "Advertise error", /* EADV */
    [69] = "Srmount error", /* ESRMNT */
    [70] = "Communication error on send", /* ECOMM */
    [71] = "Protocol error", /* EPROTO */
    [72] = "Multihop attempted", /* EMULTIHOP */
    [73] = "RFS specific error", /* EDOTDOT */
    [74] = "Bad message", /* EBADMSG */
    [75] = "Value too large for defined data type", /* EOVERFLOW */
    [76] = "Name not unique on network", /* ENOTUNIQ */
    [77] = "File descriptor in bad state", /* EBADFD */
    [78] = "Remote address changed", /* EREMCHG */
    [79] = "Can not access a needed shared library", /* ELIBACC */
    [80] = "Accessing a corrupted shared library", /* ELIBBAD */
    [81] = ".lib section in a.out corrupted", /* ELIBSCN */
    [82] = "Attempting to link in too many shared libraries", /* ELIBMAX */
    [83] = "Cannot exec a shared library directly", /* ELIBEXEC */
    [84] = "Invalid or incomplete multibyte or wide character", /* EILSEQ */
    [85] = "Interrupted system call should be restarted", /* ERESTART */
    [86] = "Streams pipe error", /* ESTRPIPE */
    [87] = "Too many users", /* EUSERS */
    [88] = "Socket operation on non-socket", /* ENOTSOCK */
    [89] = "Destination address required", /* EDESTADDRREQ */
    [90] = "Message too long", /* EMSGSIZE */
    [91] = "Protocol wrong type for socket", /* EPROTOTYPE */
    [92] = "Protocol not available", /* ENOPROTOOPT */
    [93] = "Protocol not supported", /* EPROTONOSUPPORT */
    [94] = "Socket type not supported", /* ESOCKTNOSUPPORT */
    [95] = "Operation not supported", /* ENOTSUP */
    [96] = "Protocol family not supported", /* EPFNOSUPPORT */
    [97] = "Address family not supported by protocol", /* EAFNOSUPPORT */
    [98] = "Address already in use", /* EADDRINUSE */
    [99] = "Cannot assign requested address", /* EADDRNOTAVAIL */
    [100] = "Network is down", /* ENETDOWN */
    [101] = "Network is unreachable", /* ENETUNREACH */
    [102] = "Network dropped connection on reset", /* ENETRESET */
    [103] = "Software caused connection abort", /* ECONNABORTED */
    [104] = "Connection reset by peer", /* ECONNRESET */
    [105] = "No buffer space available", /* ENOBUFS */
    [106] = "Transport endpoint is already connected", /* EISCONN */
    [107] = "Transport endpoint is not connected", /* ENOTCONN */
    [108] = "Cannot send after transport endpoint shutdown", /* ESHUTDOWN */
    [109] = "Too many references: cannot splice", /* ETOOMANYREFS */
    [110] = "Connection timed out", /* ETIMEDOUT */
    [111] = "Connection refused", /* ECONNREFUSED */
    [112] = "Host is down", /* EHOSTDOWN */
    [113] = "No route to host", /* EHOSTUNREACH */
    [114] = "Operation already in progress", /* EALREADY */
    [115] = "Operation now in progress", /* EINPROGRESS */
    [116] = "Stale file handle", /* ESTALE */
    [117] = "Structure needs cleaning", /* EUCLEAN */
    [118] = "Not a XENIX named type file", /* ENOTNAM */
    [119] = "No XENIX semaphores available", /* ENAVAIL */
    [120] = "Is a named type file", /* EISNAM */
    [121] = "Remote I/O error", /* EREMOTEIO */
    [122] = "Disk quota exceeded", /* EDQUOT */
    [123] = "No medium found", /* ENOMEDIUM */
    [124] = "Wrong medium type", /* EMEDIUMTYPE */
    [125] = "Operation canceled", /* ECANCELED */
    [126] = "Required key not available", /* ENOKEY */
    [127] = "Key has expired", /* EKEYEXPIRED */
    [128] = "Key has been revoked", /* EKEYREVOKED */
    [129] = "Key was rejected by service", /* EKEYREJECTED */
    [130] = "Owner died", /* EOWNERDEAD */
    [131] = "State not recoverable", /* ENOTRECOVERABLE */
    [132] = "Operation not possible due to RF-kill", /* ERFKILL */
    [133] = "Memory page has hardware error", /* EHWPOISON */
};

static size_t text_length(const char *s) {
    size_t n = 0;
    while (s[n]) n++;
    return n;
}

NO_BUILTIN int strerror_r(int errnum, char *buf, size_t buflen) {
    char unknown[32];
    const char *msg = 0;
    if (errnum >= 0 && (size_t)errnum < sizeof glibc_errors / sizeof glibc_errors[0]) msg = glibc_errors[errnum];
    if (!msg) {
        /* "Unknown error %d" without stdio. */
        static const char prefix[] = "Unknown error ";
        char digits[12];
        size_t nd = 0, at = 0;
        unsigned int v = errnum < 0 ? 0u - (unsigned int)errnum : (unsigned int)errnum;
        do {
            digits[nd++] = (char)('0' + v % 10);
            v /= 10;
        } while (v);
        for (size_t i = 0; prefix[i]; i++) unknown[at++] = prefix[i];
        if (errnum < 0) unknown[at++] = '-';
        while (nd) unknown[at++] = digits[--nd];
        unknown[at] = 0;
        msg = unknown;
    }
    size_t len = text_length(msg);
    if (len >= buflen) {
        if (buflen) {
            memcpy(buf, msg, buflen - 1);
            buf[buflen - 1] = 0;
        }
        return 34; /* ERANGE */
    }
    memcpy(buf, msg, len + 1);
    return 0;
}

NO_BUILTIN int __xpg_strerror_r(int errnum, char *buf, size_t buflen) {
    return strerror_r(errnum, buf, buflen);
}
