fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=CMakeLists.txt");

    let dst = cmake::Config::new(".").build();

    // `links = "neon_rt"` turns these into DEP_NEON_RT_{ROOT,INCLUDE} for
    // dependents' build scripts. No rustc-link-lib: nothing in Rust links this.
    println!("cargo:root={}", dst.display());
    println!("cargo:include={}/include", dst.display());
}
