#ifndef NEON_TIME_H
#define NEON_TIME_H

// Two clocks and a pause. `monotonic` is nanoseconds from an arbitrary origin —
// differences mean something, values do not; `unix_millis` is the calendar.

#include <stdint.h>

int64_t neon_time_monotonic(void);
int64_t neon_time_unix_millis(void);
void neon_time_sleep(int64_t millis);

#endif
