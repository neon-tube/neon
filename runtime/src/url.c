#include "libneon_rt.h"

#include "internal.h"

#include <uriparser/Uri.h>

#include <stdlib.h>
#include <string.h>

// uriparser wants NUL-terminated input; a neon_str is a view. One copy in, ranges out.

static neon_str range_str(UriTextRangeA r) {
    if (r.first == NULL) {
        return neon_str_lit("", 0);
    }
    return neon_str_new(r.first, (size_t)(r.afterLast - r.first));
}

bool neon_url_parse(neon_str s, neon_str* scheme, neon_str* host, int64_t* port,
                    neon_str* path, neon_str* query, neon_str* fragment) {
    size_t len = neon_str_len(&s);
    char* buf = malloc(len + 1);
    if (buf == NULL) {
        neon_trap("out of memory");
    }
    memcpy(buf, neon_str_data(&s), len);
    buf[len] = '\0';
    neon_str_release(s);

    UriUriA uri;
    const char* error_pos = NULL;
    bool ok = uriParseSingleUriA(&uri, buf, &error_pos) == URI_SUCCESS;
    if (ok) {
        *scheme = range_str(uri.scheme);
        *host = range_str(uri.hostText);
        if (uri.portText.first != NULL) {
            int64_t p = 0;
            for (const char* c = uri.portText.first; c != uri.portText.afterLast; c++) {
                p = p * 10 + (*c - '0'); // uriparser guaranteed digits
            }
            *port = p;
        } else {
            *port = -1;
        }
        // The raw path, rebuilt from the segment list: "/a/b" for http://h/a/b, "" for
        // http://h, "/" for http://h/ (one empty segment). Segments stay percent-encoded.
        size_t plen = 0;
        for (const UriPathSegmentA* seg = uri.pathHead; seg != NULL; seg = seg->next) {
            plen += 1 + (size_t)(seg->text.afterLast - seg->text.first);
        }
        char* pbuf = malloc(plen == 0 ? 1 : plen);
        if (pbuf == NULL) {
            neon_trap("out of memory");
        }
        size_t at = 0;
        for (const UriPathSegmentA* seg = uri.pathHead; seg != NULL; seg = seg->next) {
            pbuf[at++] = '/';
            size_t n = (size_t)(seg->text.afterLast - seg->text.first);
            memcpy(pbuf + at, seg->text.first, n);
            at += n;
        }
        *path = neon_str_new(pbuf, at);
        free(pbuf);
        *query = range_str(uri.query);
        *fragment = range_str(uri.fragment);
    }
    uriFreeUriMembersA(&uri);
    free(buf);
    return ok;
}

neon_str neon_url_decode(neon_str s) {
    size_t len = neon_str_len(&s);
    char* buf = malloc(len + 1);
    if (buf == NULL) {
        neon_trap("out of memory");
    }
    memcpy(buf, neon_str_data(&s), len);
    buf[len] = '\0';
    neon_str_release(s);
    // In place on our copy; '+' untouched, CRLF conversions off — decoding is decoding.
    const char* end = uriUnescapeInPlaceExA(buf, URI_FALSE, URI_BR_DONT_TOUCH);
    neon_str out = neon_str_new(buf, (size_t)(end - buf));
    free(buf);
    return out;
}
