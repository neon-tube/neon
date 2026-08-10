// `runtime/src/url.c`: the uriparser seam. Pins the wrapper's own contracts — component
// extraction as RAW spans, the path rebuild from segments, port digits, absence as ""/-1,
// decode-on-a-copy — not uriparser itself.

#include <string.h>

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("url");

static neon_str lit(const char* s) {
    return neon_str_lit(s, strlen(s));
}

static bool str_is(neon_str s, const char* want) {
    bool eq = neon_str_len(&s) == strlen(want) &&
              memcmp(neon_str_data(&s), want, neon_str_len(&s)) == 0;
    neon_str_release(s);
    return eq;
}

TEST(a_full_url_splits_into_raw_components) {
    neon_str scheme, host, path, query, fragment;
    int64_t port;
    EXPECT(neon_url_parse(lit("http://example.com:8080/a%20b/c?x=1&y=2#top"), &scheme,
                          &host, &port, &path, &query, &fragment));
    EXPECT(str_is(scheme, "http"));
    EXPECT(str_is(host, "example.com"));
    EXPECT_EQ(port, 8080);
    EXPECT(str_is(path, "/a%20b/c")); // RAW: still percent-encoded
    EXPECT(str_is(query, "x=1&y=2"));
    EXPECT(str_is(fragment, "top"));
}

TEST(absences_are_empty_and_minus_one) {
    neon_str scheme, host, path, query, fragment;
    int64_t port;
    EXPECT(neon_url_parse(lit("http://h"), &scheme, &host, &port, &path, &query, &fragment));
    EXPECT(str_is(scheme, "http"));
    EXPECT(str_is(host, "h"));
    EXPECT_EQ(port, -1);
    EXPECT(str_is(path, ""));
    EXPECT(str_is(query, ""));
    EXPECT(str_is(fragment, ""));
}

TEST(not_a_url_reports_false) {
    neon_str scheme, host, path, query, fragment;
    int64_t port;
    EXPECT(!neon_url_parse(lit("http://exa mple.com/"), &scheme, &host, &port, &path,
                           &query, &fragment));
}

TEST(decode_unescapes_a_copy_and_leaves_plus) {
    EXPECT(str_is(neon_url_decode(lit("/a%20b+c%2Fd")), "/a b+c/d"));
}
