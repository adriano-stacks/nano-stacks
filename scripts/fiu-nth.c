#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <fiu-control.h>
#include <fiu.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>

static _Atomic unsigned long seen;
static unsigned long target;
static const char *report;

static int fail_nth(const char *name, int *failnum, void **failinfo,
                    unsigned int *flags) {
    (void)name;
    unsigned long current = atomic_fetch_add_explicit(&seen, 1, memory_order_relaxed) + 1;
    if (current != target) {
        return 0;
    }

    int fd = open(report, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    static const char marker[] = "hit\n";
    if (fd < 0 || write(fd, marker, sizeof(marker) - 1) != (ssize_t)(sizeof(marker) - 1) ||
        close(fd) != 0) {
        _exit(125);
    }
    *failnum = 1;
    *failinfo = (void *)(intptr_t)EIO;
    *flags |= FIU_ONETIME;
    return 1;
}

__attribute__((constructor)) static void configure(void) {
    const char *point = getenv("NANO_FIU_POINT");
    const char *at = getenv("NANO_FIU_AT");
    report = getenv("NANO_FIU_REPORT");
    if (point == NULL || at == NULL || report == NULL) {
        return;
    }

    char *end = NULL;
    target = strtoul(at, &end, 10);
    if (target == 0 || end == at || *end != '\0' || fiu_init(0) != 0 ||
        fiu_enable_external(point, 1, (void *)(intptr_t)EIO, FIU_ONETIME,
                            fail_nth) != 0) {
        _exit(125);
    }
}
