fn main() {
    // rust-embed's derive has no build script of its own, so cargo never
    // notices a *new* file under assets/ and a release build keeps the
    // stale embedded snapshot (added icons render as nothing).
    println!("cargo:rerun-if-changed=../../assets");
}
