#ifdef __APPLE__
#define _DARWIN_C_SOURCE
#else
#define _GNU_SOURCE
#include <dlfcn.h>
#include <sys/syscall.h>
#endif

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <time.h>
#include <unistd.h>

#ifdef __APPLE__
#define WRAPPER(name) static int trace_##name
#define real_flock flock
#define real_close close
#else
#define WRAPPER(name) int name

static int real_flock(int fd, int flags) {
    typedef int (*function)(int, int);
    static _Atomic(function) next;
    function call = atomic_load_explicit(&next, memory_order_relaxed);
    if (!call) {
        int incoming = errno;
        call = (function)dlsym(RTLD_NEXT, "flock");
        atomic_store_explicit(&next, call, memory_order_relaxed);
        errno = incoming;
    }
    return call ? call(fd, flags) : (int)syscall(SYS_flock, fd, flags);
}

static int real_close(int fd) {
    typedef int (*function)(int);
    static _Atomic(function) next;
    function call = atomic_load_explicit(&next, memory_order_relaxed);
    if (!call) {
        int incoming = errno;
        call = (function)dlsym(RTLD_NEXT, "close");
        atomic_store_explicit(&next, call, memory_order_relaxed);
        errno = incoming;
    }
    return call ? call(fd) : (int)syscall(SYS_close, fd);
}
#endif

enum { TRACKED_FDS = 65536, WORD_BITS = 64 };
static _Atomic uint64_t seen_fds[TRACKED_FDS / WORD_BITS];

static bool mark_seen(int fd, bool remember) {
    if (fd < 0 || fd >= TRACKED_FDS) {
        return false;
    }
    uint64_t mask = UINT64_C(1) << (fd % WORD_BITS);
    _Atomic uint64_t *word = &seen_fds[fd / WORD_BITS];
    if (!remember && !(atomic_load_explicit(word, memory_order_relaxed) & mask)) {
        return false;
    }
    uint64_t old = remember ? atomic_fetch_or_explicit(word, mask, memory_order_relaxed)
                            : atomic_fetch_and_explicit(word, ~mask, memory_order_relaxed);
    return (old & mask) != 0;
}

static uint64_t now_ns(clockid_t clock) {
    struct timespec stamp;
    if (clock_gettime(clock, &stamp) != 0) {
        return 0;
    }
    return (uint64_t)stamp.tv_sec * UINT64_C(1000000000) + (uint64_t)stamp.tv_nsec;
}

static size_t describe_fd(int fd, char path[PATH_MAX]) {
#ifdef __APPLE__
    return fcntl(fd, F_GETPATH, path) == 0 ? strnlen(path, PATH_MAX) : 0;
#else
    char link[64];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t len = readlink(link, path, PATH_MAX);
    return len > 0 && len < PATH_MAX ? (size_t)len : 0;
#endif
}

static void record(const char *log, int fd, char op, int flags, uint64_t start,
                   uint64_t end, int result, int error, uint64_t cpu, const char *path, size_t path_len) {
    if (!start || end < start) {
        return;
    }
    char line[128 + 2 * PATH_MAX];
    int prefix = snprintf(line, 128, "%ld\t%d\t%c\t%d\t%" PRIu64 "\t%" PRIu64 "\t%d\t%d\t%" PRIu64 "\t",
                          (long)getpid(), fd, op, flags, start, end, result, error, cpu);
    if (prefix < 0 || prefix >= 128) {
        return;
    }
    size_t len = (size_t)prefix;
    if (path_len == 0) {
        line[len++] = '-';
    } else {
        static const char hex[] = "0123456789abcdef";
        for (size_t i = 0; i < path_len; ++i) {
            unsigned char byte = (unsigned char)path[i];
            line[len++] = hex[byte >> 4];
            line[len++] = hex[byte & 15];
        }
    }
    line[len++] = '\n';

    // Avoid keeping a descriptor the program could close or reuse.
    int sink = open(log, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);
    if (sink >= 0) {
        ssize_t written;
        do {
            written = write(sink, line, len);
        } while (written < 0 && errno == EINTR);
        (void)real_close(sink);
    }
}

WRAPPER(flock)(int fd, int flags) {
    int incoming = errno;
    const char *log = getenv("FRUST_BENCH_FLOCK_LOG");
    if (!log || !*log) {
        errno = incoming;
        return real_flock(fd, flags);
    }

    char path[PATH_MAX];
    size_t path_len = describe_fd(fd, path);
    uint64_t start = now_ns(CLOCK_MONOTONIC);
    uint64_t cpu_start = now_ns(CLOCK_THREAD_CPUTIME_ID);
    errno = incoming;
    int result = real_flock(fd, flags);
    int error = errno;
    uint64_t cpu_end = now_ns(CLOCK_THREAD_CPUTIME_ID);
    uint64_t end = now_ns(CLOCK_MONOTONIC);
    uint64_t cpu = cpu_start && cpu_end >= cpu_start ? cpu_end - cpu_start : UINT64_MAX;
    (void)mark_seen(fd, true);
    record(log, fd, 'F', flags, start, end, result, result == 0 ? 0 : error, cpu, path, path_len);
    errno = error;
    return result;
}

WRAPPER(close)(int fd) {
    int incoming = errno;
    if (!mark_seen(fd, false)) {
        return real_close(fd);
    }
    const char *log = getenv("FRUST_BENCH_FLOCK_LOG");
    if (!log || !*log) {
        errno = incoming;
        return real_close(fd);
    }
    uint64_t start = now_ns(CLOCK_MONOTONIC);
    errno = incoming;
    int result = real_close(fd);
    int error = errno;
    uint64_t end = now_ns(CLOCK_MONOTONIC);
    record(log, fd, 'C', 0, start, end, result, result == 0 ? 0 : error, 0, NULL, 0);
    errno = error;
    return result;
}

#ifdef __APPLE__
// Keep the probe in Cargo so arm64e linker tools can start with their own ABI.
__attribute__((constructor))
static void keep_trace_in_this_process(void) {
    if (getenv("FRUST_BENCH_FLOCK_LOG")) {
        unsetenv("DYLD_INSERT_LIBRARIES");
    }
}

__attribute__((used, section("__DATA,__interpose")))
static const struct {
    const void *replacement;
    const void *original;
} replacements[] = {
    {(const void *)trace_flock, (const void *)flock},
    {(const void *)trace_close, (const void *)close},
};
#endif
