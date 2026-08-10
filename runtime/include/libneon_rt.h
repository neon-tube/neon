#ifndef NEON_LIBNEON_RT_H
#define NEON_LIBNEON_RT_H

// Umbrella header: generated Neon programs include only this file.
//
// The ABI the emitted C shares with hand-written natives, one header per area of the
// runtime, mirroring the translation units in `src/`. See docs/design/ir.md.
//
// Order here is not a dependency ordering. Every header is self-contained and
// safe to include on its own — tests and CBMC models rely on that. If a build
// only works when you reorder this list, the header is missing an include.

#include "neon/any.h"
#include "neon/arena.h"
#include "neon/arith.h"
#include "neon/channel.h"
#include "neon/core.h"
#include "neon/encoding.h"
#include "neon/fiber.h"
#include "neon/file.h"
#include "neon/http.h"
#include "neon/url.h"
#include "neon/io.h"
#include "neon/lifecycle.h"
#include "neon/list.h"
#include "neon/map.h"
#include "neon/math.h"
#include "neon/net.h"
#include "neon/os.h"
#include "neon/portability.h"
#include "neon/process.h"
#include "neon/random.h"
#include "neon/resource.h"
#include "neon/string.h"
#include "neon/time.h"
#include "neon/trap.h"

#endif
