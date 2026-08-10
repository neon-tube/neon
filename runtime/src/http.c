#include "libneon_rt.h"

#include "internal.h"

#include "llhttp.h"

#include <stdlib.h>
#include <string.h>

// The llhttp accumulator (see neon/http.h for the contract).
//
// Buffers are plain malloc, not neon_alloc: they are C plumbing owned by the parser
// object, never language values, and they must not land in any fiber's arena — a parser
// can outlive a read loop's iteration but not its fiber, and the drop frees everything.

typedef struct {
    char* p;
    size_t len;
    size_t cap;
} nh_buf;

static void nh_buf_push(nh_buf* b, const char* d, size_t n) {
    if (b->len + n > b->cap) {
        size_t ncap = b->cap == 0 ? 64 : b->cap;
        while (b->len + n > ncap) {
            ncap *= 2;
        }
        char* np = realloc(b->p, ncap);
        if (np == NULL) {
            neon_trap("out of memory");
        }
        b->p = np;
        b->cap = ncap;
    }
    memcpy(b->p + b->len, d, n);
    b->len += n;
}

static void nh_buf_free(nh_buf* b) {
    free(b->p);
    b->p = NULL;
    b->len = b->cap = 0;
}

static neon_str nh_buf_str(const nh_buf* b) {
    return neon_str_new(b->p == NULL ? "" : b->p, b->len);
}

typedef struct {
    nh_buf field;
    nh_buf value;
} nh_header;

struct neon_http_parser {
    neon_header header;
    llhttp_t ll;
    bool response;
    bool complete;
    bool keep_alive; // latched at message-complete, before any reset clears llhttp's view
    nh_buf target;
    nh_buf body;
    nh_header* headers;
    size_t header_len;
    size_t header_cap;
    // llhttp splits one header across multiple callbacks; a FIELD callback after a VALUE
    // one is what starts a new pair. The classic llhttp accumulation pattern.
    bool in_value;
};

// ---- callbacks: copy the span, nothing else ----

static neon_http_parser* nh_of(llhttp_t* ll) {
    // The parser embeds the llhttp state first-member-after-header; recover the wrapper.
    return (neon_http_parser*)((char*)ll - offsetof(struct neon_http_parser, ll));
}

static int nh_on_url(llhttp_t* ll, const char* at, size_t n) {
    nh_buf_push(&nh_of(ll)->target, at, n);
    return 0;
}

static int nh_on_header_field(llhttp_t* ll, const char* at, size_t n) {
    neon_http_parser* p = nh_of(ll);
    if (p->in_value || p->header_len == 0) {
        p->in_value = false;
        if (p->header_len == p->header_cap) {
            size_t ncap = p->header_cap == 0 ? 8 : p->header_cap * 2;
            nh_header* nh = realloc(p->headers, ncap * sizeof(nh_header));
            if (nh == NULL) {
                neon_trap("out of memory");
            }
            p->headers = nh;
            p->header_cap = ncap;
        }
        memset(&p->headers[p->header_len], 0, sizeof(nh_header));
        p->header_len++;
    }
    nh_buf_push(&p->headers[p->header_len - 1].field, at, n);
    return 0;
}

static int nh_on_header_value(llhttp_t* ll, const char* at, size_t n) {
    neon_http_parser* p = nh_of(ll);
    p->in_value = true;
    if (p->header_len == 0) {
        return -1; // a value with no field is not HTTP; let llhttp error out
    }
    nh_buf_push(&p->headers[p->header_len - 1].value, at, n);
    return 0;
}

static int nh_on_body(llhttp_t* ll, const char* at, size_t n) {
    nh_buf_push(&nh_of(ll)->body, at, n);
    return 0;
}

static int nh_on_message_complete(llhttp_t* ll) {
    neon_http_parser* p = nh_of(ll);
    p->complete = true;
    p->keep_alive = llhttp_should_keep_alive(ll) != 0;
    // PAUSE, so bytes after this message (a pipelined follow-up) stay unconsumed: `feed`
    // reports where the message ended, `reset` re-arms, and the caller re-feeds the rest.
    return HPE_PAUSED;
}

static const llhttp_settings_t* nh_settings(void) {
    static llhttp_settings_t s;
    static bool init = false;
    if (!init) {
        llhttp_settings_init(&s);
        s.on_url = nh_on_url;
        s.on_header_field = nh_on_header_field;
        s.on_header_value = nh_on_header_value;
        s.on_body = nh_on_body;
        s.on_message_complete = nh_on_message_complete;
        init = true;
    }
    return &s;
}

static void nh_clear(neon_http_parser* p) {
    nh_buf_free(&p->target);
    nh_buf_free(&p->body);
    for (size_t i = 0; i < p->header_len; i++) {
        nh_buf_free(&p->headers[i].field);
        nh_buf_free(&p->headers[i].value);
    }
    free(p->headers);
    p->headers = NULL;
    p->header_len = p->header_cap = 0;
    p->complete = false;
    p->keep_alive = false;
    p->in_value = false;
}

static void neon_http_parser_drop(void* pv) {
    neon_http_parser* p = (neon_http_parser*)pv;
    nh_clear(p);
    neon_free(p);
}

neon_http_parser* neon_http_parser_new(bool response) {
    neon_http_parser* p = (neon_http_parser*)neon_alloc(
        sizeof(struct neon_http_parser) - sizeof(neon_header), neon_http_parser_drop);
    memset((char*)p + sizeof(neon_header), 0, sizeof(struct neon_http_parser) - sizeof(neon_header));
    p->response = response;
    llhttp_init(&p->ll, response ? HTTP_RESPONSE : HTTP_REQUEST, nh_settings());
    return p;
}

int64_t neon_http_parser_feed(neon_http_parser* p, neon_str data, int64_t* consumed) {
    int64_t code;
    llhttp_errno_t err = llhttp_execute(&p->ll, neon_str_data(&data), neon_str_len(&data));
    if (err == HPE_OK) {
        *consumed = (int64_t)neon_str_len(&data);
        code = NEON_HTTP_MORE;
    } else if (err == HPE_PAUSED && p->complete) {
        // Paused at message-complete: everything up to the pause point was consumed.
        *consumed = (int64_t)(llhttp_get_error_pos(&p->ll) - neon_str_data(&data));
        code = NEON_HTTP_COMPLETE;
    } else {
        *consumed = 0;
        code = NEON_HTTP_ERROR;
    }
    neon_str_release(data);
    neon_release((neon_header*)p);
    return code;
}

neon_str neon_http_parser_method(neon_http_parser* p) {
    neon_str s = p->response ? neon_str_lit("", 0)
                             : neon_str_new(llhttp_method_name(llhttp_get_method(&p->ll)),
                                            strlen(llhttp_method_name(llhttp_get_method(&p->ll))));
    neon_release((neon_header*)p);
    return s;
}

neon_str neon_http_parser_target(neon_http_parser* p) {
    neon_str s = nh_buf_str(&p->target);
    neon_release((neon_header*)p);
    return s;
}

int64_t neon_http_parser_status(neon_http_parser* p) {
    int64_t s = (int64_t)llhttp_get_status_code(&p->ll);
    neon_release((neon_header*)p);
    return s;
}

neon_str neon_http_parser_body(neon_http_parser* p) {
    neon_str s = nh_buf_str(&p->body);
    neon_release((neon_header*)p);
    return s;
}

bool neon_http_parser_keep_alive(neon_http_parser* p) {
    bool k = p->keep_alive;
    neon_release((neon_header*)p);
    return k;
}

int64_t neon_http_parser_header_count(neon_http_parser* p) {
    int64_t n = (int64_t)p->header_len;
    neon_release((neon_header*)p);
    return n;
}

neon_str neon_http_parser_header_at(neon_http_parser* p, int64_t i, neon_str* value) {
    neon_str f;
    if (i < 0 || (size_t)i >= p->header_len) {
        f = neon_str_lit("", 0);
        *value = neon_str_lit("", 0);
    } else {
        f = nh_buf_str(&p->headers[i].field);
        *value = nh_buf_str(&p->headers[i].value);
    }
    neon_release((neon_header*)p);
    return f;
}

neon_str neon_http_parser_error(neon_http_parser* p) {
    const char* reason = llhttp_get_error_reason(&p->ll);
    const char* name = llhttp_errno_name(llhttp_get_errno(&p->ll));
    neon_str s;
    if (reason != NULL && reason[0] != '\0') {
        s = neon_str_new(reason, strlen(reason));
    } else {
        s = neon_str_new(name, strlen(name));
    }
    neon_release((neon_header*)p);
    return s;
}

void neon_http_parser_reset(neon_http_parser* p) {
    nh_clear(p);
    llhttp_reset(&p->ll);
    neon_release((neon_header*)p);
}

// End-of-input, for EOF-terminated response bodies (no Content-Length, connection
// close): tells llhttp the peer is done, which completes such a message. COMPLETE when a
// message finished (now or earlier), ERROR when the bytes so far cannot end here.
int64_t neon_http_parser_finish(neon_http_parser* p) {
    llhttp_errno_t err = llhttp_finish(&p->ll);
    int64_t code;
    if (p->complete) {
        code = NEON_HTTP_COMPLETE; // finish's own PAUSED from our callback is completion
    } else if (err == HPE_OK) {
        code = NEON_HTTP_MORE;
    } else {
        code = NEON_HTTP_ERROR;
    }
    neon_release((neon_header*)p);
    return code;
}
