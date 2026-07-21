cmake_minimum_required(VERSION 3.15)
project(neon_rt C)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)

# One translation unit per area of the runtime, each with the matching public header in
# `include/neon/`. `include/libneon_rt.h` is the umbrella the emitted C includes, so a
# program never has to know this layout; `src/internal.h` carries the bits only these
# files share and is not installed.
set(NEON_RT_SOURCES
    src/lifecycle.c
    src/trap.c
    src/arith.c
    src/string.c
    src/resource.c
    src/file.c
    src/math.c
    src/io.c
    src/list.c
    src/any.c
    src/map.c
)

# The runtime ships as prebuilt archives, one per build shape the CLI can ask for, so a
# Neon build links a `.a` instead of recompiling the whole runtime every time (and a
# shipped toolchain need not ship the runtime's C source). The flags here must stay in
# step with `BuildConfig::cc_args` in `cli/src/buildcfg.rs`, which builds the *program*
# with the matching shape; `cli/src/buildcfg.rs::runtime_variant` is what picks between
# them.
#
# Two deliberate differences from what `cc_args` would have produced when the runtime was
# compiled from source alongside the program:
#
#   * `-flto` only as FAT objects, and only where the compiler supports them. The
#     objection to a plain `-flto` archive stands: it carries compiler-version-specific
#     bitcode and nothing else, and the compiler that built the toolchain is not the `cc`
#     that links a user's program. `-ffat-lto-objects` emits both -- bitcode *and* real
#     machine code -- so an LTO-capable matching `cc` inlines through the archive and any
#     other compiler links the `.text` exactly as before. Verified on this archive: the
#     objects carry `.gnu.lto_*` sections and a populated `.text`.
#
#     It is worth the archive size. The runtime's primitives are small and called from
#     the innermost loops a program has -- `neon_list_at` is four lines and a bounds
#     check -- so leaving them un-inlinable costs more than it sounds: on the brainfuck
#     benchmark (10^8 ops) an un-inlinable runtime ran 3.66s against 1.08s for the same
#     program compiled whole, and `neon_list_at` alone was 22% of the profile.
#   * No `-march=native` (see the `OptRelease` note in `runtime_variant`): a prebuilt
#     archive is built once, for an unknown target.
#
# Also: `cflags`/`-D` a user passes reach their program only. They no longer reach the
# runtime, because the runtime is no longer compiled per build.
# Fat LTO objects are a GCC spelling; clang has historically not accepted the flag. Probe
# rather than assume, and fall back to no LTO at all rather than to a plain `-flto` -- a
# bitcode-only archive is the thing that cannot be linked by a different compiler, which
# is the whole reason this is conditional.
include(CheckCCompilerFlag)
check_c_compiler_flag("-flto -ffat-lto-objects" NEON_RT_HAS_FAT_LTO)
if(NEON_RT_HAS_FAT_LTO)
    set(NEON_RT_LTO -flto -ffat-lto-objects)
else()
    set(NEON_RT_LTO)
endif()

function(neon_rt_variant name)
    add_library(${name} STATIC ${NEON_RT_SOURCES})
    target_include_directories(${name} PUBLIC include)
    # -Werror on every variant: the runtime is warning-clean in all three shapes, and a
    # variant that is only ever built in CI is exactly where rot would go unnoticed.
    target_compile_options(${name} PRIVATE -Wall -Wextra -Werror ${ARGN})
    install(TARGETS ${name} ARCHIVE DESTINATION lib)
endfunction()

# `release`: what `Mode::Release` implies, minus `-flto` (above). Also what `OptRelease`
# links.
neon_rt_variant(neon_rt -O3 ${NEON_RT_LTO})
# `debug`: `Mode::Debug`. `NEON_DEBUG` turns on the runtime's own assertions.
neon_rt_variant(neon_rt_debug -O0 -g -DNEON_DEBUG)
# `sanitized`: the only variant that carries instrumentation. ASan does not see a
# heap-buffer-overflow that happens inside code compiled without `-fsanitize`, so an
# uninstrumented archive in a sanitized program is a silent hole -- the CLI refuses to
# fall back to one rather than produce a build that covers half the program.
neon_rt_variant(neon_rt_san -fsanitize=address,undefined -O1 -g -fno-omit-frame-pointer)

install(DIRECTORY include/ DESTINATION include)

# ---- unit tests ----
#
# Off by default, and that default is load-bearing: `neon-runtime`'s build script drives
# this CMakeLists on every `cargo build`, and the tests pull tinyunit over the network
# (FetchContent). That does not belong in a routine build. CI configures with
# `-DNEON_RT_TESTS=ON` to build and run them; nothing else touches them.
#
# The runtime sources are compiled *into* the test binary rather than linked from an archive,
# so the tests can reach `src/internal.h` and the non-exported helpers, and the sanitizers
# below instrument the runtime itself. Not `-Werror` here: that is the archive's contract,
# not the test build's, and the test flags (sanitizers, -O1) are a different shape.
option(NEON_RT_TESTS "Build the C runtime's tinyunit unit tests" OFF)
if(NEON_RT_TESTS)
    enable_testing()

    # tinyunit runs each test in its own forked process. A crash or trap fails only that test
    # (the runner reports it and continues), a precondition-violation trap (`_exit(101)`) is
    # asserted with EXPECT_TRAP, and — because the child exits normally — LeakSanitizer runs
    # per test and fails the exact test that leaked, with no framework support code.
    #
    # Fetch the source but skip tinyunit's own build (SOURCE_SUBDIR points at a directory with
    # no CMakeLists, so MakeAvailable populates without add_subdirectory): tinyunit.c is
    # compiled straight into the test binary below, like the runtime sources, so one set of
    # sanitizer flags instruments everything.
    include(FetchContent)
    FetchContent_Declare(
        tinyunit
        GIT_REPOSITORY https://github.com/jkbbwr/tinyunit
        GIT_TAG        main
        SOURCE_SUBDIR  do-not-build
    )
    FetchContent_MakeAvailable(tinyunit)

    add_executable(neon_rt_tests
        ${NEON_RT_SOURCES}
        ${tinyunit_SOURCE_DIR}/tinyunit.c
        tests/init.c
        tests/arith_test.c
        tests/lifecycle_test.c
        tests/string_test.c
        tests/list_test.c
        tests/map_test.c
        tests/resource_test.c
        tests/any_test.c
        tests/math_test.c
    )
    # The rich assertion and TEST macros use GNU C (__auto_type, constructors, _Generic).
    set_target_properties(neon_rt_tests PROPERTIES C_STANDARD 11 C_EXTENSIONS ON)
    target_include_directories(neon_rt_tests PRIVATE include src ${tinyunit_SOURCE_DIR})
    target_link_libraries(neon_rt_tests PRIVATE m)

    # Sanitizers on the whole test binary — the runtime sources and tinyunit compiled in with
    # it — so a memory or UB bug in the runtime is caught here, not only in the corpus.
    # `-fno-sanitize-recover=all` so a UBSan finding aborts (non-zero exit) rather than
    # printing and continuing — UBSan otherwise recovers by default and the run stays green.
    set(NEON_RT_TEST_SAN -fsanitize=address,undefined -fno-sanitize-recover=all)
    target_compile_options(neon_rt_tests PRIVATE ${NEON_RT_TEST_SAN} -g -O1 -fno-omit-frame-pointer)
    target_link_options(neon_rt_tests PRIVATE ${NEON_RT_TEST_SAN})

    add_test(NAME neon_rt_tests COMMAND neon_rt_tests)
endif()
