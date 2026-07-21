use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=CMakeLists.txt");

    // One full archive set per compiler *family*, under `<OUT_DIR>/<flavor>/lib/`. The
    // motivation is measured, not theoretical: LTO bitcode does not cross families, so a
    // clang-linked program against gcc-built fat objects silently falls back to the
    // machine code and loses every cross-archive inline — 2.8s vs 0.7s on the n-body
    // benchmark — and the sanitizer runtimes do not mix across families at all. The CLI
    // picks the flavor matching the `cc` doing the final link
    // (`cli/src/buildcfg.rs::cc_flavor`).
    //
    // Identification is by `--version`, not by name: on macOS `gcc` *is* clang, and
    // building the same compiler twice under two names would stage a lie. A family whose
    // compiler is absent is skipped — the CLI reports a missing flavor at link time,
    // naming what this machine had when the toolchain was built.
    // `-march=native` in the runtime archives is opt-in and only sound from source: this
    // build script runs on the machine that will run the program, so tuning for its CPU is
    // safe here in a way it never is for a shipped archive. `NEON_RT_NATIVE` in the
    // environment turns it on; the release CI that packages downloadable archives leaves it
    // unset. `CMakeLists.txt` still probes that the compiler accepts the flag.
    println!("cargo:rerun-if-env-changed=NEON_RT_NATIVE");
    let native = std::env::var_os("NEON_RT_NATIVE").is_some();

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let mut built: Vec<&str> = Vec::new();
    for flavor in ["gcc", "clang"] {
        if !identifies_as(flavor, flavor) {
            continue;
        }
        // See the previous revision's note, still load-bearing: the runtime's variants
        // carry hand-picked flags (`CMakeLists.txt`), so cmake must not inject its own.
        cmake::Config::new(".")
            .no_default_flags(true)
            .define("CMAKE_BUILD_TYPE", "None")
            .define("CMAKE_C_COMPILER", flavor)
            .define("NEON_RT_NATIVE", if native { "ON" } else { "OFF" })
            .out_dir(out.join(flavor))
            .build();
        built.push(flavor);
    }
    assert!(
        !built.is_empty(),
        "neither `gcc` nor `clang` is on PATH; the runtime cannot be built"
    );

    // `links = "neon_rt"` turns these into DEP_NEON_RT_{ROOT,INCLUDE} for dependents'
    // build scripts. ROOT holds one subdirectory per flavor built; the headers are
    // compiler-independent, so INCLUDE points into whichever flavor exists.
    println!("cargo:root={}", out.display());
    println!("cargo:include={}/{}/include", out.display(), built[0]);
}

/// Whether running `cc --version` says the compiler is the named family.
fn identifies_as(cc: &str, family: &str) -> bool {
    let Ok(output) = std::process::Command::new(cc).arg("--version").output() else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
    match family {
        "clang" => text.contains("clang"),
        // gcc must positively identify, not merely "not clang": on macOS `gcc` is clang.
        _ => text.contains("gcc") && !text.contains("clang"),
    }
}
