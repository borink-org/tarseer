//! Asks for `advapi32` on Windows. The allocator calls into it there and does
//! not ask for it itself.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=advapi32");
    }
}
