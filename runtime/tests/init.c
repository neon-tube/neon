// Runtime setup that must happen before any test runs. tinyunit provides `main`, and a
// global constructor runs before it, so `neon_rt_init` is called once ahead of every suite.
// It is no longer quite a no-op -- on Windows it puts the standard streams in binary mode,
// which `io_test.c` counts bytes against -- and calling it keeps the tests honest about the
// runtime's contract: initialise, then use.
//
// The GNU constructor attribute, deliberately unabstracted. tinyunit's own `TEST` macro
// registers every test through the same attribute, so a translation unit that could not use
// it could not define a test either; wrapping it here would suggest this file is the thing
// standing between the suite and a compiler that lacks it, and it is not. mingw-w64's gcc
// and clang both have it, and so does clang-cl. cl.exe does not, and tinyunit's `TEST` does
// not need it to: it plants a pointer in `.CRT$XCU` there instead. So this line, not that
// macro, is what would have to grow an MSVC spelling before this suite could build under
// cl.exe — the opposite of what an earlier version of this comment claimed.

#include "libneon_rt.h"

__attribute__((constructor)) static void neon_rt_test_init(void) { neon_rt_init(); }
