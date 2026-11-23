/*
 * iouring-probe: can this process create an io_uring? Exit 0 = yes, 1 = EPERM, 2 = other error.
 * Built static so the same binary runs inside the bench container and on the host (D1 checks).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    unsigned char params[120]; /* struct io_uring_params */
    memset(params, 0, sizeof params);
    long fd = syscall(__NR_io_uring_setup, 4, params);
    if (fd >= 0) { close((int)fd); puts("io_uring: allowed"); return 0; }
    printf("io_uring: %s\n", strerror(errno));
    return errno == EPERM ? 1 : 2;
}
