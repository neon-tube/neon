// `runtime/src/http.c`: the accumulating llhttp wrapper behind `std::http`. What this
// pins: the accumulator's own work (header pairing across split callbacks, chunked-body
// reassembly, the pause-at-complete/consumed contract that keeps pipelined bytes intact,
// reset re-arming) — not llhttp itself, which has its own suite upstream.

#include <string.h>

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("http");

static neon_str lit(const char* s) {
    return neon_str_lit(s, strlen(s));
}

static bool str_is(neon_str s, const char* want) {
    bool eq = neon_str_len(&s) == strlen(want) &&
              memcmp(neon_str_data(&s), want, neon_str_len(&s)) == 0;
    neon_str_release(s);
    return eq;
}

TEST(a_request_with_headers_and_body_accumulates) {
    neon_http_parser* p = neon_http_parser_new(false);
    const char* req =
        "POST /submit?x=1 HTTP/1.1\r\n"
        "Host: example.com\r\n"
        "Content-Length: 5\r\n"
        "\r\n"
        "hello";
    int64_t consumed = 0;
    neon_retain((neon_header*)p);
    EXPECT_EQ(neon_http_parser_feed(p, lit(req), &consumed), NEON_HTTP_COMPLETE);
    EXPECT_EQ(consumed, (int64_t)strlen(req));

    neon_retain((neon_header*)p);
    EXPECT(str_is(neon_http_parser_method(p), "POST"));
    neon_retain((neon_header*)p);
    EXPECT(str_is(neon_http_parser_target(p), "/submit?x=1"));
    neon_retain((neon_header*)p);
    EXPECT(str_is(neon_http_parser_body(p), "hello"));
    neon_retain((neon_header*)p);
    EXPECT_EQ(neon_http_parser_header_count(p), 2);
    neon_str v;
    neon_retain((neon_header*)p);
    EXPECT(str_is(neon_http_parser_header_at(p, 0, &v), "Host"));
    EXPECT(str_is(v, "example.com"));
    neon_retain((neon_header*)p);
    EXPECT(neon_http_parser_keep_alive(p));
    neon_release((neon_header*)p);
}

TEST(a_chunked_response_reassembles_and_split_feeds_work) {
    neon_http_parser* p = neon_http_parser_new(true);
    // Fed in two arbitrary pieces, split inside a chunk: the accumulator must not care.
    const char* part1 =
        "HTTP/1.1 200 OK\r\n"
        "Transfer-Encoding: chunked\r\n"
        "\r\n"
        "5\r\nhel";
    const char* part2 = "lo\r\n6\r\n world\r\n0\r\n\r\n";
    int64_t consumed = 0;
    neon_retain((neon_header*)p);
    EXPECT_EQ(neon_http_parser_feed(p, lit(part1), &consumed), NEON_HTTP_MORE);
    EXPECT_EQ(consumed, (int64_t)strlen(part1));
    neon_retain((neon_header*)p);
    EXPECT_EQ(neon_http_parser_feed(p, lit(part2), &consumed), NEON_HTTP_COMPLETE);

    neon_retain((neon_header*)p);
    EXPECT_EQ(neon_http_parser_status(p), 200);
    neon_retain((neon_header*)p);
    EXPECT(str_is(neon_http_parser_body(p), "hello world"));
    neon_release((neon_header*)p);
}

TEST(pipelined_bytes_survive_reset_and_refeed) {
    neon_http_parser* p = neon_http_parser_new(false);
    const char* one = "GET /a HTTP/1.1\r\nHost: h\r\n\r\n";
    const char* two = "GET /b HTTP/1.1\r\nHost: h\r\n\r\n";
    char both[128];
    snprintf(both, sizeof(both), "%s%s", one, two);

    int64_t consumed = 0;
    neon_retain((neon_header*)p);
    EXPECT_EQ(neon_http_parser_feed(p, lit(both), &consumed), NEON_HTTP_COMPLETE);
    EXPECT_EQ(consumed, (int64_t)strlen(one)); // the second request is NOT swallowed
    neon_retain((neon_header*)p);
    EXPECT(str_is(neon_http_parser_target(p), "/a"));

    neon_retain((neon_header*)p);
    neon_http_parser_reset(p);
    neon_retain((neon_header*)p);
    EXPECT_EQ(neon_http_parser_feed(p, lit(both + consumed), &consumed), NEON_HTTP_COMPLETE);
    neon_retain((neon_header*)p);
    EXPECT(str_is(neon_http_parser_target(p), "/b"));
    neon_release((neon_header*)p);
}

TEST(garbage_reports_an_error_with_a_reason) {
    neon_http_parser* p = neon_http_parser_new(false);
    int64_t consumed = 0;
    neon_retain((neon_header*)p);
    EXPECT_EQ(neon_http_parser_feed(p, lit("this is not http\r\n\r\n"), &consumed),
              NEON_HTTP_ERROR);
    neon_retain((neon_header*)p);
    neon_str e = neon_http_parser_error(p);
    EXPECT(neon_str_len(&e) > 0);
    neon_str_release(e);
    neon_release((neon_header*)p);
}

TEST(connection_close_reports_no_keep_alive) {
    neon_http_parser* p = neon_http_parser_new(false);
    const char* req = "GET / HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n";
    int64_t consumed = 0;
    neon_retain((neon_header*)p);
    EXPECT_EQ(neon_http_parser_feed(p, lit(req), &consumed), NEON_HTTP_COMPLETE);
    neon_retain((neon_header*)p);
    EXPECT(!neon_http_parser_keep_alive(p));
    neon_release((neon_header*)p);
}
