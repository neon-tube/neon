// Child processes for `std::process`: spawn with per-stream pipes, a poll-pumped
// capture, wait, and a hard kill.
//
// The failure channel is `-errno`, like every IO native, and the one failure that needs
// machinery is exec's: it happens in the CHILD, after fork, where the parent cannot see
// it. The classic fix is used — a CLOEXEC pipe the child writes its errno into; a
// successful exec closes it unread, so "the pipe had bytes" IS "the spawn failed", and
// `run("no-such-program")` throws instead of producing an empty success.
//
// Child output crosses the same lossy UTF-8 boundary as argv and env (`neon_os_lossy`):
// bytes with no promises become `str` with each invalid byte replaced.
//
// Windows: every native returns `-ENOSYS`, the same honest refusal fs's directory
// natives started with. The API is CreateProcess + anonymous pipes + a threaded pump —
// real work, recorded in TODO.md, not faked here.

#include "libneon_rt.h"

#include "platform.h"

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32

int64_t neon_proc_spawn(neon_str program, neon_list* args, neon_list* envs, neon_str cwd,
                        int64_t in_mode, int64_t out_mode, int64_t err_mode, int64_t* in_fd,
                        int64_t* out_fd, int64_t* err_fd) {
    neon_str_release(program);
    neon_release((neon_header*)args);
    neon_release((neon_header*)envs);
    neon_str_release(cwd);
    (void)in_mode;
    (void)out_mode;
    (void)err_mode;
    *in_fd = -1;
    *out_fd = -1;
    *err_fd = -1;
    return -ENOSYS;
}

int64_t neon_proc_run(neon_str program, neon_list* args, neon_list* envs, neon_str cwd,
                      neon_str input, neon_str* out, neon_str* err_s) {
    neon_str_release(program);
    neon_release((neon_header*)args);
    neon_release((neon_header*)envs);
    neon_str_release(cwd);
    neon_str_release(input);
    *out = neon_str_lit("", 0);
    *err_s = neon_str_lit("", 0);
    return -ENOSYS;
}

int64_t neon_proc_wait(int64_t pid) {
    (void)pid;
    return -ENOSYS;
}

int64_t neon_proc_kill(int64_t pid) {
    (void)pid;
    return -ENOSYS;
}

neon_str neon_proc_read_some(int64_t fd, int64_t max, int64_t* err) {
    (void)fd;
    (void)max;
    *err = -ENOSYS;
    return neon_str_lit("", 0);
}

#else

#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>

extern char** environ;

// A NUL-terminated copy of a `neon_str`'s bytes; every exec-family argument needs one.
static char* dup_cstr(const char* p, size_t n) {
    char* s = malloc(n + 1);
    if (s == NULL) {
        neon_trap("out of memory");
    }
    memcpy(s, p, n);
    s[n] = '\0';
    return s;
}

static void free_vec(char** v) {
    if (v == NULL) {
        return;
    }
    for (char** p = v; *p != NULL; p++) {
        free(*p);
    }
    free(v);
}

// argv for exec: the program, then each element of `args`, NULL-terminated. Borrowed
// views into the list are copied — exec wants NUL-terminated strings and a `neon_str`
// is not.
static char** build_argv(neon_str* program, neon_list* args) {
    int64_t n = (int64_t)args->len;
    char** v = calloc((size_t)n + 2, sizeof(char*));
    if (v == NULL) {
        neon_trap("out of memory");
    }
    v[0] = dup_cstr(neon_str_data(program), neon_str_len(program));
    for (int64_t i = 0; i < n; i++) {
        neon_str* a = (neon_str*)neon_list_at(args, i);
        v[i + 1] = dup_cstr(neon_str_data(a), neon_str_len(a));
    }
    return v;
}

// envp: the parent's environment with `envs` ("KEY=VALUE" strings) EXTENDING it — an
// override wins over an inherited variable of the same name. Extension rather than
// replacement because "run it with one variable set" is the case that occurs; a
// from-scratch environment can be a later option, and building it from `os::env` is
// possible today.
static bool same_key(const char* a, const char* b) {
    const char* ae = strchr(a, '=');
    const char* be = strchr(b, '=');
    if (ae == NULL || be == NULL || (ae - a) != (be - b)) {
        return false;
    }
    return strncmp(a, b, (size_t)(ae - a)) == 0;
}

static char** build_envp(neon_list* envs) {
    int64_t extra = (int64_t)envs->len;
    size_t inherited = 0;
    for (char** e = environ; *e != NULL; e++) {
        inherited++;
    }
    char** v = calloc(inherited + (size_t)extra + 1, sizeof(char*));
    if (v == NULL) {
        neon_trap("out of memory");
    }
    size_t at = 0;
    for (char** e = environ; *e != NULL; e++) {
        bool overridden = false;
        for (int64_t i = 0; i < extra && !overridden; i++) {
            neon_str* o = (neon_str*)neon_list_at(envs, i);
            char* oc = dup_cstr(neon_str_data(o), neon_str_len(o));
            overridden = same_key(*e, oc);
            free(oc);
        }
        if (!overridden) {
            v[at++] = dup_cstr(*e, strlen(*e));
        }
    }
    for (int64_t i = 0; i < extra; i++) {
        neon_str* o = (neon_str*)neon_list_at(envs, i);
        v[at++] = dup_cstr(neon_str_data(o), neon_str_len(o));
    }
    return v;
}

// Stream modes, matching std::process's atoms via the wrapper's translation.
#define NEON_STREAM_INHERIT 0
#define NEON_STREAM_PIPED 1
#define NEON_STREAM_NULL 2

// One stream's plumbing: `parent_end`/`child_end` for a pipe, or the /dev/null fd, or
// nothing for inherit. `read_side` says which pipe end the CHILD gets.
typedef struct {
    int64_t mode;
    int child_end;  // what dup2s onto the child's fd; -1 for inherit
    int parent_end; // what the parent keeps; -1 unless piped
} stream_plan;

static int plan_stream(stream_plan* p, int64_t mode, bool child_reads) {
    p->mode = mode;
    p->child_end = -1;
    p->parent_end = -1;
    if (mode == NEON_STREAM_PIPED) {
        int fds[2];
        if (pipe(fds) != 0) {
            return -1;
        }
        p->child_end = child_reads ? fds[0] : fds[1];
        p->parent_end = child_reads ? fds[1] : fds[0];
    } else if (mode == NEON_STREAM_NULL) {
        p->child_end = open("/dev/null", O_RDWR);
        if (p->child_end < 0) {
            return -1;
        }
    }
    return 0;
}

static void close_if(int fd) {
    if (fd >= 0) {
        close(fd);
    }
}

// The spawn. Returns the pid, or `-errno` — including an errno from the CHILD's own
// chdir or exec, read back through the CLOEXEC pipe.
int64_t neon_proc_spawn(neon_str program, neon_list* args, neon_list* envs, neon_str cwd,
                        int64_t in_mode, int64_t out_mode, int64_t err_mode, int64_t* in_fd,
                        int64_t* out_fd, int64_t* err_fd) {
    *in_fd = -1;
    *out_fd = -1;
    *err_fd = -1;

    char** argv = build_argv(&program, args);
    char** envp = build_envp(envs);
    char* cwd_c = neon_str_len(&cwd) > 0 ? dup_cstr(neon_str_data(&cwd), neon_str_len(&cwd)) : NULL;
    neon_str_release(program);
    neon_release((neon_header*)args);
    neon_release((neon_header*)envs);
    neon_str_release(cwd);

    // All -1 up front: a `plan_stream` that fails short-circuits the `||` chain, leaving
    // later plans unassigned, and the cleanup below closes all three unconditionally.
    stream_plan in = {0, -1, -1}, out = {0, -1, -1}, err = {0, -1, -1};
    int64_t failed = 0;
    int report[2] = {-1, -1};
    if (plan_stream(&in, in_mode, true) != 0 || plan_stream(&out, out_mode, false) != 0
        || plan_stream(&err, err_mode, false) != 0 || pipe(report) != 0
        || fcntl(report[1], F_SETFD, FD_CLOEXEC) != 0) {
        failed = -(int64_t)errno;
    }

    pid_t pid = -1;
    if (failed == 0) {
        pid = fork();
        if (pid < 0) {
            failed = -(int64_t)errno;
        }
    }

    if (pid == 0) {
        // The child. Nothing here may allocate or touch the runtime: between fork and
        // exec only async-signal-safe calls are dependable.
        close(report[0]);
        if (in.child_end >= 0 && dup2(in.child_end, 0) < 0) {
            goto child_fail;
        }
        if (out.child_end >= 0 && dup2(out.child_end, 1) < 0) {
            goto child_fail;
        }
        if (err.child_end >= 0 && dup2(err.child_end, 2) < 0) {
            goto child_fail;
        }
        close_if(in.child_end);
        close_if(out.child_end);
        close_if(err.child_end);
        close_if(in.parent_end);
        close_if(out.parent_end);
        close_if(err.parent_end);
        if (cwd_c != NULL && chdir(cwd_c) != 0) {
            goto child_fail;
        }
        environ = envp;
        execvp(argv[0], argv);
    child_fail:;
        int e = errno;
        ssize_t w = write(report[1], &e, sizeof e);
        (void)w;
        _exit(127);
    }

    // The parent.
    free_vec(argv);
    free_vec(envp);
    free(cwd_c);
    close_if(in.child_end);
    close_if(out.child_end);
    close_if(err.child_end);
    close_if(report[1]);

    if (failed != 0) {
        close_if(in.parent_end);
        close_if(out.parent_end);
        close_if(err.parent_end);
        close_if(report[0]);
        return failed;
    }

    // Exec succeeded exactly when the report pipe closes empty.
    int child_errno = 0;
    ssize_t got = read(report[0], &child_errno, sizeof child_errno);
    close(report[0]);
    if (got > 0) {
        close_if(in.parent_end);
        close_if(out.parent_end);
        close_if(err.parent_end);
        waitpid(pid, NULL, 0); // the failed child exits 127 at once; reap it
        return -(int64_t)child_errno;
    }

    *in_fd = in.parent_end;
    *out_fd = out.parent_end;
    *err_fd = err.parent_end;
    return (int64_t)pid;
}

// Exit status as ONE integer, the shell's convention: the code for a normal exit,
// 128 + the signal for a signal death (SIGKILL is 137 everywhere CI reads numbers).
static int64_t status_code(int status) {
    if (WIFEXITED(status)) {
        return (int64_t)WEXITSTATUS(status);
    }
    if (WIFSIGNALED(status)) {
        return 128 + (int64_t)WTERMSIG(status);
    }
    return 128; // stopped/continued cannot reach a plain waitpid
}

int64_t neon_proc_wait(int64_t pid) {
    int status = 0;
    if (waitpid((pid_t)pid, &status, 0) < 0) {
        return -(int64_t)errno; // ECHILD: already waited, or never ours
    }
    return status_code(status);
}

int64_t neon_proc_kill(int64_t pid) {
    // SIGKILL, deliberately: the one signal with a Windows meaning (TerminateProcess)
    // and no handler to hang in. Graceful shutdown is closing the child's stdin and
    // waiting — a convention, not a signal.
    if (kill((pid_t)pid, SIGKILL) != 0) {
        return -(int64_t)errno;
    }
    return 0;
}

// One read(2), up to `max` bytes (clamped to 1 MiB): the streaming primitive. "" with
// no error is end-of-stream. A chunk boundary can split a UTF-8 sequence, and the lossy
// conversion then replaces the split halves — a documented cost of reading a text
// stream in chunks.
neon_str neon_proc_read_some(int64_t fd, int64_t max, int64_t* err) {
    *err = 0;
    if (max <= 0) {
        return neon_str_lit("", 0);
    }
    if (max > 1048576) {
        max = 1048576;
    }
    char* buf = malloc((size_t)max);
    if (buf == NULL) {
        neon_trap("out of memory");
    }
    ssize_t got = read((int)fd, buf, (size_t)max);
    if (got < 0) {
        free(buf);
        *err = -(int64_t)errno;
        return neon_str_lit("", 0);
    }
    neon_str s = neon_os_lossy(buf, (size_t)got);
    free(buf);
    return s;
}

// A growing capture buffer for the pump.
typedef struct {
    char* data;
    size_t len, cap;
} grow_buf;

static void grow_init(grow_buf* b) {
    b->cap = 4096;
    b->len = 0;
    b->data = malloc(b->cap);
    if (b->data == NULL) {
        neon_trap("out of memory");
    }
}

static void grow_put(grow_buf* b, const char* p, size_t n) {
    while (b->len + n > b->cap) {
        b->cap *= 2;
        char* g = realloc(b->data, b->cap);
        if (g == NULL) {
            neon_trap("out of memory");
        }
        b->data = g;
    }
    memcpy(b->data + b->len, p, n);
    b->len += n;
}

// Spawn with everything piped, pump stdin out and stdout/stderr in with poll() until
// both outputs close, then reap. The pump is WHY `run` cannot deadlock: a child that
// fills one pipe while this side is busy with another is always drained, which no
// sequential read-then-read can promise. Returns the exit code, or `-errno` when the
// spawn itself failed.
int64_t neon_proc_run(neon_str program, neon_list* args, neon_list* envs, neon_str cwd,
                      neon_str input, neon_str* out, neon_str* err_s) {
    *out = neon_str_lit("", 0);
    *err_s = neon_str_lit("", 0);

    int64_t in_fd, out_fd, err_fd;
    int64_t pid = neon_proc_spawn(program, args, envs, cwd, NEON_STREAM_PIPED,
                                  NEON_STREAM_PIPED, NEON_STREAM_PIPED, &in_fd, &out_fd, &err_fd);
    if (pid < 0) {
        neon_str_release(input);
        return pid;
    }

    const char* in_data = neon_str_data(&input);
    size_t in_len = neon_str_len(&input), in_at = 0;
    if (in_len == 0) {
        close((int)in_fd);
        in_fd = -1;
    }

    grow_buf ob, eb;
    grow_init(&ob);
    grow_init(&eb);
    char chunk[65536];

    while (out_fd >= 0 || err_fd >= 0 || in_fd >= 0) {
        struct pollfd fds[3];
        int n = 0;
        int oi = -1, ei = -1, ii = -1;
        if (out_fd >= 0) {
            oi = n;
            fds[n++] = (struct pollfd){(int)out_fd, POLLIN, 0};
        }
        if (err_fd >= 0) {
            ei = n;
            fds[n++] = (struct pollfd){(int)err_fd, POLLIN, 0};
        }
        if (in_fd >= 0) {
            ii = n;
            fds[n++] = (struct pollfd){(int)in_fd, POLLOUT, 0};
        }
        if (poll(fds, (nfds_t)n, -1) < 0) {
            if (errno == EINTR) {
                continue;
            }
            break;
        }
        if (oi >= 0 && (fds[oi].revents & (POLLIN | POLLHUP))) {
            ssize_t got = read((int)out_fd, chunk, sizeof chunk);
            if (got <= 0) {
                close((int)out_fd);
                out_fd = -1;
            } else {
                grow_put(&ob, chunk, (size_t)got);
            }
        }
        if (ei >= 0 && (fds[ei].revents & (POLLIN | POLLHUP))) {
            ssize_t got = read((int)err_fd, chunk, sizeof chunk);
            if (got <= 0) {
                close((int)err_fd);
                err_fd = -1;
            } else {
                grow_put(&eb, chunk, (size_t)got);
            }
        }
        if (ii >= 0 && (fds[ii].revents & (POLLOUT | POLLERR | POLLHUP))) {
            ssize_t put = write((int)in_fd, in_data + in_at, in_len - in_at);
            if (put < 0) {
                // EPIPE: the child stopped reading. Its business; stop feeding it.
                close((int)in_fd);
                in_fd = -1;
            } else {
                in_at += (size_t)put;
                if (in_at == in_len) {
                    close((int)in_fd);
                    in_fd = -1;
                }
            }
        }
    }
    neon_str_release(input);

    int64_t code = neon_proc_wait(pid);
    neon_str o = neon_os_lossy(ob.data, ob.len);
    neon_str e = neon_os_lossy(eb.data, eb.len);
    free(ob.data);
    free(eb.data);
    *out = o;
    *err_s = e;
    return code;
}

#endif
