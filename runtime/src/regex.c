#include "libneon_rt.h"

#include "internal.h"

// The 8-bit interpreted PCRE2. Width and static linkage are pinned here rather than left to
// the build so this translation unit reads correctly on its own; the vendored library is
// compiled with the matching -DPCRE2_CODE_UNIT_WIDTH=8 (see runtime/CMakeLists.txt).
#define PCRE2_CODE_UNIT_WIDTH 8
#define PCRE2_STATIC
#include <pcre2.h>

#include <stdlib.h>
#include <string.h>

// A compiled pattern: a plain refcounted value wrapping one `pcre2_code`. `code` is NULL
// only in the object a failed compile hands back, which the language throws on before ever
// using — its drop still runs and frees nothing.
struct neon_regex {
    neon_header header;
    pcre2_code* code;
};

static void neon_regex_drop(void* pv) {
    neon_regex* r = (neon_regex*)pv;
    if (r->code != NULL) {
        pcre2_code_free(r->code);
    }
    neon_free(r);
}

// One match's ovector, copied out so the transient `pcre2_match_data` can be freed at once.
// `ov` is plain malloc (C plumbing owned by the object, never a language value); `pairs` is
// how many (start,end) pairs it holds — group 0 plus every numbered group.
struct neon_regex_match {
    neon_header header;
    bool matched;
    uint32_t pairs;
    PCRE2_SIZE* ov;
};

static void neon_regex_match_drop(void* pv) {
    neon_regex_match* m = (neon_regex_match*)pv;
    free(m->ov);
    neon_free(m);
}

neon_regex* neon_regex_compile(neon_str pattern, bool* ok, int64_t* offset, neon_str* message) {
    int errnum = 0;
    PCRE2_SIZE erroff = 0;
    pcre2_code* code = pcre2_compile((PCRE2_SPTR)neon_str_data(&pattern),
                                     neon_str_len(&pattern), PCRE2_UTF | PCRE2_UCP, &errnum,
                                     &erroff, NULL);
    neon_regex* r = (neon_regex*)neon_alloc(sizeof(struct neon_regex) - sizeof(neon_header),
                                            neon_regex_drop);
    r->code = code;
    if (code == NULL) {
        *ok = false;
        *offset = (int64_t)erroff;
        // PCRE2 renders its own diagnostic; a fixed buffer is what the API expects.
        PCRE2_UCHAR buf[256];
        int n = pcre2_get_error_message(errnum, buf, sizeof buf / sizeof buf[0]);
        *message = neon_str_new((const char*)buf, n >= 0 ? (size_t)n : 0);
    } else {
        *ok = true;
        *offset = -1;
        *message = neon_str_lit("", 0);
    }
    neon_str_release(pattern);
    return r;
}

bool neon_regex_is_match(neon_regex* re, neon_str subject) {
    // A one-pair match data is enough: is_match asks only whether a match exists.
    pcre2_match_data* md = pcre2_match_data_create(1, NULL);
    int rc = pcre2_match(re->code, (PCRE2_SPTR)neon_str_data(&subject), neon_str_len(&subject), 0,
                         0, md, NULL);
    pcre2_match_data_free(md);
    neon_str_release(subject);
    neon_release((neon_header*)re);
    return rc >= 0;
}

int64_t neon_regex_group_count(neon_regex* re) {
    uint32_t capture_count = 0;
    pcre2_pattern_info(re->code, PCRE2_INFO_CAPTURECOUNT, &capture_count);
    neon_release((neon_header*)re);
    // + 1 for group 0, the whole match.
    return (int64_t)capture_count + 1;
}

neon_regex_match* neon_regex_exec(neon_regex* re, neon_str subject, int64_t start) {
    pcre2_match_data* md = pcre2_match_data_create_from_pattern(re->code, NULL);
    int rc = pcre2_match(re->code, (PCRE2_SPTR)neon_str_data(&subject), neon_str_len(&subject),
                         (PCRE2_SIZE)start, 0, md, NULL);

    neon_regex_match* m = (neon_regex_match*)neon_alloc(
        sizeof(struct neon_regex_match) - sizeof(neon_header), neon_regex_match_drop);
    if (rc < 0) {
        m->matched = false;
        m->pairs = 0;
        m->ov = NULL;
    } else {
        uint32_t pairs = pcre2_get_ovector_count(md);
        const PCRE2_SIZE* ov = pcre2_get_ovector_pointer(md);
        m->matched = true;
        m->pairs = pairs;
        m->ov = (PCRE2_SIZE*)malloc((size_t)pairs * 2 * sizeof(PCRE2_SIZE));
        if (m->ov == NULL) {
            neon_trap("out of memory");
        }
        memcpy(m->ov, ov, (size_t)pairs * 2 * sizeof(PCRE2_SIZE));
    }

    pcre2_match_data_free(md);
    neon_str_release(subject);
    neon_release((neon_header*)re);
    return m;
}

bool neon_regex_match_group(neon_regex_match* m, int64_t group, int64_t* start, int64_t* end) {
    bool participated = false;
    int64_t s = -1;
    int64_t e = -1;
    if (m->matched && group >= 0 && (uint32_t)group < m->pairs) {
        PCRE2_SIZE gs = m->ov[2 * (size_t)group];
        PCRE2_SIZE ge = m->ov[2 * (size_t)group + 1];
        if (gs != PCRE2_UNSET) {
            participated = true;
            s = (int64_t)gs;
            e = (int64_t)ge;
        }
    }
    *start = s;
    *end = e;
    neon_release((neon_header*)m);
    return participated;
}

neon_str neon_regex_substitute(neon_regex* re, neon_str subject, neon_str replacement,
                               bool global) {
    uint32_t options = PCRE2_SUBSTITUTE_OVERFLOW_LENGTH | PCRE2_SUBSTITUTE_UNSET_EMPTY;
    if (global) {
        options |= PCRE2_SUBSTITUTE_GLOBAL;
    }

    PCRE2_SPTR subj = (PCRE2_SPTR)neon_str_data(&subject);
    PCRE2_SIZE subjlen = neon_str_len(&subject);
    PCRE2_SPTR repl = (PCRE2_SPTR)neon_str_data(&replacement);
    PCRE2_SIZE repllen = neon_str_len(&replacement);

    // A first guess big enough for the common no-growth case; OVERFLOW_LENGTH reports the
    // exact size when it is too small, so at most one retry follows.
    PCRE2_SIZE outlen = subjlen + repllen + 32;
    PCRE2_UCHAR* buf = (PCRE2_UCHAR*)malloc(outlen);
    if (buf == NULL) {
        neon_trap("out of memory");
    }
    int rc = pcre2_substitute(re->code, subj, subjlen, 0, options, NULL, NULL, repl, repllen, buf,
                              &outlen);
    if (rc == PCRE2_ERROR_NOMEMORY) {
        // `outlen` now holds the required size, terminating NUL included.
        free(buf);
        buf = (PCRE2_UCHAR*)malloc(outlen);
        if (buf == NULL) {
            neon_trap("out of memory");
        }
        rc = pcre2_substitute(re->code, subj, subjlen, 0, options, NULL, NULL, repl, repllen, buf,
                              &outlen);
    }
    if (rc < 0) {
        // A malformed replacement (a stray `$`, a bad reference) is the only way here now;
        // name it loudly rather than return the subject and hide the mistake.
        neon_trap("regex replacement is malformed");
    }
    neon_str out = neon_str_new((const char*)buf, outlen);
    free(buf);

    neon_str_release(subject);
    neon_str_release(replacement);
    neon_release((neon_header*)re);
    return out;
}
