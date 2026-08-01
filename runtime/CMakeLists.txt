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
#   * `-flto`, as FAT objects where the compiler emits them and plain bitcode otherwise.
#     `-ffat-lto-objects` emits both -- bitcode *and* real machine code -- so an LTO-capable
#     matching `cc` inlines through the archive and *any other* compiler still links the
#     `.text`. That last property matters only across compiler *families*: a gcc-built
#     bitcode-only archive cannot be linked by clang at all (it is not machine code and the
#     bitcode dialects do not cross). But the archives are split by family already
#     (`lib/<flavor>/`, chosen by `cli/src/buildcfg.rs::cc_flavor`), so the clang archive is
#     only ever linked by a clang `cc` -- which means a bitcode-only `-flto` archive is
#     perfectly linkable there, and refusing LTO to a clang that lacks fat objects (Apple
#     Clang rejects the flag on arm64) throws away the entire inlining win for insurance the
#     flavor split already provides. So: fat objects when available, plain `-flto` when only
#     that is (still family-locked, still linkable), no LTO only when neither is.
#
#     The one place the flavor split does not hold is `Sysroot::runtime_lib`'s fallback,
#     which links the other family's archive when a family is missing entirely (on macOS
#     only `clang` is staged at all, so a mac user with real gcc lands there). A plain
#     `-flto` archive has no machine code for that link to fall back to, so the CLI checks
#     for it and refuses with an explanation rather than handing bitcode to a linker that
#     cannot read it.
#
#     The one residual is cross-*version* within a family, in the prebuilt-download install
#     path: CI's clang builds the archive and the user's clang links it. LLVM bitcode
#     auto-upgrades across versions, so this degrades gracefully rather than hard-failing
#     like the cross-family case; and the from-source install (`cargo build --release`,
#     `NEON_BUILD_FROM_SOURCE`) uses one compiler for both, where plain `-flto` is exact.
#
#     It is worth the archive size. The runtime's primitives are small and called from
#     the innermost loops a program has -- `neon_list_at` is four lines and a bounds
#     check -- so leaving them un-inlinable costs more than it sounds: on the brainfuck
#     benchmark (10^8 ops) an un-inlinable runtime ran 3.66s against 1.08s for the same
#     program compiled whole (2.21s vs 0.87s measured again on arm64/Apple Clang, which is
#     exactly the flavor the no-LTO fallback used to strand), and `neon_list_at` alone was
#     22% of the profile.
#   * No `-march=native` by default: a prebuilt archive is built once, for a target it does
#     not know, and native code for the build machine SIGILLs on a user's older CPU. But a
#     from-source install *does* know its target is the machine it runs on, so
#     `-DNEON_RT_NATIVE=ON` (set by build.rs when `NEON_RT_NATIVE` is in the environment)
#     opts in. The release CI that produces downloadable archives never sets it.
#
# Also: `cflags`/`-D` a user passes reach their program only. They no longer reach the
# runtime, because the runtime is no longer compiled per build.
include(CheckCCompilerFlag)
# Not probed under MSVC: `cl.exe` answers an unrecognised `-flto` with warning D9002 and
# keeps going, so the probe would come back *true* and then quietly do nothing. Guarding is
# the only way to make "does this compiler have LTO" mean what it says here.
if(NOT MSVC)
    check_c_compiler_flag("-flto -ffat-lto-objects" NEON_RT_HAS_FAT_LTO)
    check_c_compiler_flag("-flto" NEON_RT_HAS_LTO)
endif()
if(NEON_RT_HAS_FAT_LTO)
    set(NEON_RT_LTO -flto -ffat-lto-objects)
elseif(NEON_RT_HAS_LTO)
    set(NEON_RT_LTO -flto)
else()
    set(NEON_RT_LTO)
endif()

# `-march=native` only when a from-source build opts in (see the note above): safe because
# such a build targets the machine compiling it, never safe for a shipped archive. Its own
# variable, not appended to `NEON_RT_LTO`: they are independent switches, and one that
# hides inside the other's name is one nobody finds again.
option(NEON_RT_NATIVE "Build the runtime archives with -march=native (from-source only)" OFF)
set(NEON_RT_ARCH)
if(NEON_RT_NATIVE AND NOT MSVC)
    check_c_compiler_flag("-march=native" NEON_RT_HAS_NATIVE)
    if(NEON_RT_HAS_NATIVE)
        set(NEON_RT_ARCH -march=native)
    endif()
endif()

# The three variants' flags, named once and chosen by compiler family, because MSVC shares
# none of the spellings. What each variant *means* is the same on both sides, and is
# documented at the `neon_rt_variant` calls below; only the vocabulary differs.
#
# mingw-w64 targets Windows with a GNU driver, so it takes the GNU branch -- the split here
# is on the compiler, not on the operating system, and `MSVC` is exactly that test.
#
# `MSVC` is true for anything driving the `cl` command line, which includes clang-cl, and
# that is the reason to keep this branch rather than the reason to distrust it: clang-cl is
# the only MSVC-ABI toolchain that can build the *test suite*, because tinyunit registers
# tests through `__attribute__((constructor))` and clang-cl has it while cl.exe does not.
# The `NEON_MSVC` macros in `include/neon/portability.h` deliberately do NOT fire for
# clang-cl -- it takes the GNU spellings there -- so this branch and that header answer two
# different questions and are meant to disagree about clang-cl.
if(MSVC)
    # `_CRT_SECURE_NO_WARNINGS`: MSVC deprecates `strerror` (and friends) in favour of its
    # own `_s` variants, at a level `/WX` turns into an error. `neon_io_strerror` renders a
    # code into a fresh `neon_str` and never touches a caller's buffer, so the overrun the
    # deprecation is about cannot happen here; taking the non-portable `strerror_s` to
    # silence it would trade real portability for an imagined bounds win.
    #
    # `/wd4204` and `/wd4221`: MSVC calls a non-constant aggregate initializer a
    # "nonstandard extension". It was one -- in C89. `neon_str s = {data, len, h};` is
    # ordinary C99, the runtime is built `-std=c11`, and the alternative is field-by-field
    # assignment everywhere a struct is built from locals.
    set(NEON_RT_WARN /W4 /WX /wd4204 /wd4221 -D_CRT_SECURE_NO_WARNINGS)
    set(NEON_RT_FLAGS_RELEASE /O2)
    set(NEON_RT_FLAGS_DEBUG /Od /Zi -DNEON_DEBUG)
    # ASan only: MSVC ships no UndefinedBehaviorSanitizer, so this variant covers strictly
    # less on MSVC than elsewhere. It is still the instrumented archive a sanitized build
    # must link -- an uninstrumented one would be a silent hole -- but it is not the same
    # coverage, and a report of "clean under the sanitized runtime" means less here.
    set(NEON_RT_FLAGS_SAN /fsanitize=address /Od /Zi)
else()
    set(NEON_RT_WARN -Wall -Wextra -Werror)
    set(NEON_RT_FLAGS_RELEASE -O3 ${NEON_RT_LTO} ${NEON_RT_ARCH})
    set(NEON_RT_FLAGS_DEBUG -O0 -g -DNEON_DEBUG)
    set(NEON_RT_FLAGS_SAN -fsanitize=address,undefined -O1 -g -fno-omit-frame-pointer)
endif()

# The sanitized variant is the one shape a compiler can flatly decline: mingw-w64's gcc
# ships no libasan and no libubsan, so on that toolchain there is nothing to instrument
# with. Probed rather than assumed, and probed with the link step included, because
# `-fsanitize` is a link flag as much as a compile one and a compile-only probe passes on a
# toolchain that then cannot link.
#
# When it is absent the target is not defined at all. That is deliberate: an archive that
# quietly lost its instrumentation is worse than one that is missing, because the CLI
# refuses to link a variant that is not there and has no way to tell that one which *is*
# there covers nothing.
set(CMAKE_REQUIRED_LINK_OPTIONS ${NEON_RT_FLAGS_SAN})
string(REPLACE ";" " " NEON_RT_SAN_PROBE "${NEON_RT_FLAGS_SAN}")
check_c_compiler_flag("${NEON_RT_SAN_PROBE}" NEON_RT_HAS_SAN)
unset(CMAKE_REQUIRED_LINK_OPTIONS)

function(neon_rt_variant name)
    add_library(${name} STATIC ${NEON_RT_SOURCES})
    target_include_directories(${name} PUBLIC include)
    # Warnings-as-errors on every variant: the runtime is warning-clean in all three
    # shapes, and a variant that is only ever built in CI is exactly where rot would go
    # unnoticed.
    target_compile_options(${name} PRIVATE ${NEON_RT_WARN} ${ARGN})
    install(TARGETS ${name} ARCHIVE DESTINATION lib)
endfunction()

# `release`: what `Mode::Release` implies, minus `-flto` (above). Also what `OptRelease`
# links.
# `-march=native` (when opted in) rides on this variant alone: the other two exist to be
# debugged, where the build machine's widest instructions buy nothing.
neon_rt_variant(neon_rt ${NEON_RT_FLAGS_RELEASE})
# `debug`: `Mode::Debug`. `NEON_DEBUG` turns on the runtime's own assertions.
neon_rt_variant(neon_rt_debug ${NEON_RT_FLAGS_DEBUG})
# `sanitized`: the only variant that carries instrumentation. ASan does not see a
# heap-buffer-overflow that happens inside code compiled without `-fsanitize`, so an
# uninstrumented archive in a sanitized program is a silent hole -- the CLI refuses to
# fall back to one rather than produce a build that covers half the program.
if(NEON_RT_HAS_SAN)
    neon_rt_variant(neon_rt_san ${NEON_RT_FLAGS_SAN})
else()
    message(STATUS
        "${CMAKE_C_COMPILER_ID} on this platform has no sanitizer runtime; the sanitized "
        "archive will not be built and `neon build --sanitize` will refuse rather than "
        "link an uninstrumented one.")
endif()

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
#
# On Windows this suite builds, configures and RUNS like anywhere else, with one thing
# missing: mingw-w64 ships no sanitizer runtime, so the probe below drops ASan/UBSan and
# with them the per-test leak check. Windows proves the suite runs; Linux is where it runs
# instrumented.
#
# The per-test sub-process POSIX gets by forking is a re-exec there — tinyunit runs its own
# binary again with a filter and reads the verdict off the child's exit code. That is
# tinyunit's work, and it landed; nothing on this side of the line had to change for it,
# because the temp files, the stream capture and the constructors were already portable.
# The one consequence worth knowing: a re-executed child REBUILDS the state a forked child
# would have inherited, so a death test whose body is not deterministic up to the assertion
# would behave differently on the two platforms.
option(NEON_RT_TESTS "Build the C runtime's tinyunit unit tests" OFF)
if(NEON_RT_TESTS)
    enable_testing()

    # tinyunit runs each test in its own process (forked on POSIX, re-executed on Windows).
    # A crash or trap fails
    # only that test (the runner reports it and continues), a precondition-violation trap
    # (`_exit(101)`) is asserted with EXPECT_TRAP, and — because the child exits normally —
    # LeakSanitizer runs per test and fails the exact test that leaked, with no framework
    # support code.
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
        tests/io_test.c
        tests/file_test.c
        tests/trap_test.c
        tests/core_test.c
    )
    # The rich assertion and TEST macros use GNU C (__auto_type, constructors, _Generic).
    set_target_properties(neon_rt_tests PROPERTIES C_STANDARD 11 C_EXTENSIONS ON)
    target_include_directories(neon_rt_tests PRIVATE include src ${tinyunit_SOURCE_DIR})
    # `std::math` bottoms out in libm, a separate library on Linux. Not on Windows, where
    # the math functions are in the CRT and there is no `m` to find.
    if(NOT WIN32)
        target_link_libraries(neon_rt_tests PRIVATE m)
    endif()

    # Sanitizers on the whole test binary — the runtime sources and tinyunit compiled in with
    # it — so a memory or UB bug in the runtime is caught here, not only in the corpus.
    # `-fno-sanitize-recover=all` so a UBSan finding aborts (non-zero exit) rather than
    # printing and continuing — UBSan otherwise recovers by default and the run stays green.
    #
    # Gated on the same probe the sanitized archive uses, because mingw-w64 ships neither
    # libasan nor libubsan and the link would simply fail. The suite still runs there; what
    # it loses is worth naming rather than discovering. Above all it loses LeakSanitizer,
    # which is how a leak is attributed to the exact test that leaked -- tinyunit does
    # nothing to detect leaks itself, it just honours the child's exit status, and without
    # LSan nothing sets that status. A green run on an unsanitized toolchain says the
    # assertions held, not that the runtime is leak-free.
    set(NEON_RT_TEST_SAN -fsanitize=address,undefined -fno-sanitize-recover=all)
    if(NEON_RT_HAS_SAN)
        target_compile_options(neon_rt_tests PRIVATE ${NEON_RT_TEST_SAN} -g -O1 -fno-omit-frame-pointer)
        target_link_options(neon_rt_tests PRIVATE ${NEON_RT_TEST_SAN})
    else()
        target_compile_options(neon_rt_tests PRIVATE -g -O1 -fno-omit-frame-pointer)
        message(STATUS
            "neon_rt_tests: no sanitizer runtime on this toolchain, so the suite runs "
            "uninstrumented — no ASan, no UBSan, and no per-test leak check.")
    endif()

    # Run from the binary directory: the tests compose temp-file names in the working
    # directory (`nt_temp_path`), and a leftover from a killed run belongs in the build tree
    # rather than wherever ctest happened to be invoked from.
    add_test(NAME neon_rt_tests COMMAND neon_rt_tests
             WORKING_DIRECTORY ${CMAKE_CURRENT_BINARY_DIR})
endif()
