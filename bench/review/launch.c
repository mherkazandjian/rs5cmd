/*
 * launch: run one command pinned to a CPU set and report what it cost, as one JSON line.
 *
 *   launch [-c CPUS] [-g NAME=CPUS]... [-o FILE] [-t SECONDS] -- command [args...]
 *
 *   -c CPUS        affinity for the command (e.g. 6,7,14,15), set in the child before exec
 *   -g NAME=CPUS   report busy / softirq / irq seconds of these host CPUs over the run
 *                  (from /proc/stat; loopback network work is charged to no process, so the
 *                  client and server cpusets are read separately)
 *   -o FILE        append the JSON line to FILE (default: stderr)
 *   -t SECONDS     kill the command (SIGKILL) after this long; reported as "timed_out"
 *
 * Wall time comes from CLOCK_MONOTONIC around fork/wait4; user, sys, max RSS, faults and
 * context switches from wait4's rusage. Built static so the forked pre-exec image is tiny:
 * ru_maxrss can never be lower than that image (calibrate with /bin/true).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MAXCPU 256
#define MAXG 8

typedef struct { unsigned long long v[10]; } cpustat; /* user nice system idle iowait irq softirq steal guest guest_nice */

static int parse_cpus(const char *s, unsigned char *mask) {
    memset(mask, 0, MAXCPU);
    while (*s) {
        char *end;
        long a = strtol(s, &end, 10), b = a;
        if (end == s) return -1;
        if (*end == '-') { s = end + 1; b = strtol(s, &end, 10); if (end == s) return -1; }
        for (long i = a; i <= b && i < MAXCPU; i++) mask[i] = 1;
        s = end;
        if (*s == ',') s++;
        else if (*s) return -1;
    }
    return 0;
}

static int read_stat(cpustat *st) {
    FILE *f = fopen("/proc/stat", "r");
    if (!f) return -1;
    char line[512];
    memset(st, 0, sizeof(cpustat) * MAXCPU);
    while (fgets(line, sizeof line, f)) {
        int n;
        if (strncmp(line, "cpu", 3) || line[3] < '0' || line[3] > '9') continue;
        char *p = line + 3;
        n = (int)strtol(p, &p, 10);
        if (n < 0 || n >= MAXCPU) continue;
        for (int k = 0; k < 10; k++) st[n].v[k] = strtoull(p, &p, 10);
    }
    fclose(f);
    return 0;
}

static volatile pid_t child = -1;
static volatile int timed_out = 0;
static void on_alarm(int sig) { (void)sig; timed_out = 1; if (child > 0) kill(child, SIGKILL); }

int main(int argc, char **argv) {
    const char *cpus = NULL, *out = NULL;
    const char *gname[MAXG]; unsigned char gmask[MAXG][MAXCPU]; int ng = 0;
    double timeout = 0;
    int i = 1;
    for (; i < argc; i++) {
        if (!strcmp(argv[i], "--")) { i++; break; }
        if (!strcmp(argv[i], "-c") && i + 1 < argc) cpus = argv[++i];
        else if (!strcmp(argv[i], "-o") && i + 1 < argc) out = argv[++i];
        else if (!strcmp(argv[i], "-t") && i + 1 < argc) timeout = atof(argv[++i]);
        else if (!strcmp(argv[i], "-g") && i + 1 < argc && ng < MAXG) {
            char *spec = argv[++i], *eq = strchr(spec, '=');
            if (!eq) { fprintf(stderr, "launch: bad -g %s\n", spec); return 125; }
            *eq = 0; gname[ng] = spec;
            if (parse_cpus(eq + 1, gmask[ng])) { fprintf(stderr, "launch: bad cpus %s\n", eq + 1); return 125; }
            ng++;
        } else { fprintf(stderr, "launch: bad argument %s\n", argv[i]); return 125; }
    }
    if (i >= argc) { fprintf(stderr, "usage: launch [-c CPUS] [-g NAME=CPUS]... [-o FILE] [-t S] -- cmd...\n"); return 125; }

    cpu_set_t set; CPU_ZERO(&set);
    if (cpus) {
        unsigned char m[MAXCPU];
        if (parse_cpus(cpus, m)) { fprintf(stderr, "launch: bad cpus %s\n", cpus); return 125; }
        for (int k = 0; k < MAXCPU; k++) if (m[k]) CPU_SET(k, &set);
    }

    static cpustat s0[MAXCPU], s1[MAXCPU];
    struct timespec t0, t1;
    if (ng) read_stat(s0);
    clock_gettime(CLOCK_MONOTONIC, &t0);
    pid_t pid = fork();
    if (pid < 0) { perror("launch: fork"); return 125; }
    if (pid == 0) {
        if (cpus && sched_setaffinity(0, sizeof set, &set)) { perror("launch: sched_setaffinity"); _exit(126); }
        execvp(argv[i], argv + i);
        perror("launch: exec");
        _exit(127);
    }
    child = pid;
    if (timeout > 0) { signal(SIGALRM, on_alarm); alarm((unsigned)(timeout + 0.999)); }
    int status; struct rusage ru;
    while (wait4(pid, &status, 0, &ru) < 0) if (errno != EINTR) { perror("launch: wait4"); return 125; }
    clock_gettime(CLOCK_MONOTONIC, &t1);
    if (ng) read_stat(s1);

    FILE *o = out ? fopen(out, "a") : stderr;
    if (!o) { perror("launch: open output"); return 125; }
    double hz = (double)sysconf(_SC_CLK_TCK);
    fprintf(o, "{\"wall_s\":%.6f,\"user_s\":%.6f,\"sys_s\":%.6f,\"maxrss_kb\":%ld,"
               "\"minflt\":%ld,\"majflt\":%ld,\"nvcsw\":%ld,\"nivcsw\":%ld,"
               "\"exit\":%d,\"signal\":%d,\"timed_out\":%s",
            (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9,
            ru.ru_utime.tv_sec + ru.ru_utime.tv_usec / 1e6, ru.ru_stime.tv_sec + ru.ru_stime.tv_usec / 1e6,
            ru.ru_maxrss, ru.ru_minflt, ru.ru_majflt, ru.ru_nvcsw, ru.ru_nivcsw,
            WIFEXITED(status) ? WEXITSTATUS(status) : -1, WIFSIGNALED(status) ? WTERMSIG(status) : 0,
            timed_out ? "true" : "false");
    if (ng) {
        fprintf(o, ",\"cpusets\":{");
        for (int g = 0; g < ng; g++) {
            double d[10] = {0};
            for (int k = 0; k < MAXCPU; k++)
                if (gmask[g][k]) for (int f = 0; f < 10; f++) d[f] += (double)(s1[k].v[f] - s0[k].v[f]) / hz;
            double busy = d[0] + d[1] + d[2] + d[5] + d[6] + d[7];
            fprintf(o, "%s\"%s\":{\"busy_s\":%.2f,\"user_s\":%.2f,\"sys_s\":%.2f,\"irq_s\":%.2f,"
                       "\"softirq_s\":%.2f,\"idle_s\":%.2f,\"iowait_s\":%.2f}",
                    g ? "," : "", gname[g], busy, d[0] + d[1], d[2], d[5], d[6], d[3], d[4]);
        }
        fprintf(o, "}");
    }
    fprintf(o, "}\n");
    if (out) fclose(o);
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}
