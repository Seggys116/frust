// CPU-time sampler that attributes a process's samples to the mapped file (DSO) containing the
// interrupted instruction. Used where perf is unavailable (perf_event_paranoid > 2) to measure
// how much of rustc's CPU time is spent in libLLVM versus librustc_driver and libc.
//
// Build: clang -O2 -shared -fPIC -o dso-sampler.so dso-sampler.c
// Use:   DSO_SAMPLER_OUT=/path/prefix LD_PRELOAD=/path/dso-sampler.so rustc ...
// Only processes whose executable name contains DSO_SAMPLER_MATCH (default "rustc") sample;
// each writes PREFIX.<pid> with "<samples> <path>" lines at exit. ITIMER_PROF counts process CPU
// time across all threads, at 1 kHz.
#define _GNU_SOURCE
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <ucontext.h>
#include <unistd.h>

#define MAX_SAMPLES (1u << 22)
static uintptr_t samples[MAX_SAMPLES];
static atomic_uint sample_count;
static int active;

static void on_prof(int sig, siginfo_t *info, void *context) {
    (void)sig;
    (void)info;
    ucontext_t *uc = context;
    unsigned index = atomic_fetch_add_explicit(&sample_count, 1, memory_order_relaxed);
    if (index < MAX_SAMPLES) samples[index] = (uintptr_t)uc->uc_mcontext.gregs[REG_RIP];
}

__attribute__((constructor)) static void start(void) {
    const char *match = getenv("DSO_SAMPLER_MATCH");
    char exe[4096];
    ssize_t length = readlink("/proc/self/exe", exe, sizeof exe - 1);
    if (length <= 0 || !getenv("DSO_SAMPLER_OUT")) return;
    exe[length] = 0;
    const char *base = strrchr(exe, '/');
    if (!strstr(base ? base + 1 : exe, match ? match : "rustc")) return;
    struct sigaction action = {0};
    action.sa_sigaction = on_prof;
    action.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&action.sa_mask);
    sigaction(SIGPROF, &action, NULL);
    struct itimerval timer = {{0, 1000}, {0, 1000}};
    setitimer(ITIMER_PROF, &timer, NULL);
    active = 1;
}

struct mapping {
    uintptr_t start, end;
    char path[512];
    unsigned long hits;
};

__attribute__((destructor)) static void finish(void) {
    if (!active) return;
    struct itimerval off = {{0, 0}, {0, 0}};
    setitimer(ITIMER_PROF, &off, NULL);
    static struct mapping maps[8192];
    size_t count = 0;
    FILE *file = fopen("/proc/self/maps", "r");
    if (!file) return;
    char line[1024];
    while (count < 8192 && fgets(line, sizeof line, file)) {
        unsigned long start, end;
        char perms[8], path[512] = "[anon]";
        if (sscanf(line, "%lx-%lx %7s %*s %*s %*s %511[^\n]", &start, &end, perms, path) < 3)
            continue;
        if (!strchr(perms, 'x')) continue;
        maps[count].start = start;
        maps[count].end = end;
        snprintf(maps[count].path, sizeof maps[count].path, "%s", path);
        maps[count].hits = 0;
        count++;
    }
    fclose(file);
    unsigned total = atomic_load(&sample_count);
    if (total > MAX_SAMPLES) total = MAX_SAMPLES;
    unsigned long unknown = 0;
    for (unsigned i = 0; i < total; i++) {
        size_t j;
        for (j = 0; j < count; j++)
            if (samples[i] >= maps[j].start && samples[i] < maps[j].end) break;
        if (j < count) maps[j].hits++; else unknown++;
    }
    char out[4096];
    snprintf(out, sizeof out, "%s.%d", getenv("DSO_SAMPLER_OUT"), (int)getpid());
    FILE *sink = fopen(out, "w");
    if (!sink) return;
    for (size_t j = 0; j < count; j++)
        if (maps[j].hits) fprintf(sink, "%lu %s\n", maps[j].hits, maps[j].path);
    if (unknown) fprintf(sink, "%lu [unmapped]\n", unknown);
    fclose(sink);
}
