//! The build links the prebuilt `libduckdb.so` (see `.cargo/config.toml`),
//! which libduckdb-sys copies into `target/<profile>/deps`. Cargo puts that
//! directory on the library path for `cargo run` and `cargo test` only, so
//! point the binaries at it as well. They then also start when run directly,
//! or under sudo, which drops the environment.

fn main() {
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/deps");
    println!("cargo:rustc-link-arg-examples=-Wl,-rpath,$ORIGIN/../deps");
    println!("cargo:rerun-if-changed=build.rs");
}
