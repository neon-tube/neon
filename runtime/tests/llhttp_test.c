// llhttp, pulled by FetchContent pinned to a release tag (`CMakeLists.txt`), so this
// suite pins only that the pull and the link worked — one canned request through
// `llhttp_execute`, not a re-test of llhttp itself.

#include <string.h>

#include "tinyunit.h"

#include "llhttp.h"

TEST_SUITE("llhttp");

TEST(parses_a_canned_get_request) {
    llhttp_t parser;
    llhttp_settings_t settings;
    llhttp_settings_init(&settings);
    llhttp_init(&parser, HTTP_REQUEST, &settings);

    const char* req = "GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
    llhttp_errno_t err = llhttp_execute(&parser, req, strlen(req));
    EXPECT_EQ(err, HPE_OK);
    EXPECT_EQ(llhttp_get_method(&parser), (uint8_t)HTTP_GET);
    EXPECT_EQ(llhttp_get_http_major(&parser), 1);
    EXPECT_EQ(llhttp_get_http_minor(&parser), 1);
}
