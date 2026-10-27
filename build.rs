//! Static musl builds only (docs/RELEASING.md): compile src/musl_string.c,
//! faster memcmp/bcmp/memcpy/memmove than musl's, and link it as a whole
//! archive ahead of libc.a. Other targets build nothing here.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/musl_string.c");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("musl") {
        return;
    }
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    cc::Build::new()
        .file("src/musl_string.c")
        .opt_level(2)
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag_if_supported("-fno-stack-protector")
        .warnings(true)
        .extra_warnings(true)
        .cargo_metadata(false)
        .compile("bazel_rs_musl_string");
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static:+whole-archive=bazel_rs_musl_string");
}
