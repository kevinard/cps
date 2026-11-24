#define _GNU_SOURCE

#include <err.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/random.h>
#include <sys/reboot.h>
#include <sys/utsname.h>
#include <unistd.h>

struct state {
    uint64_t nonce;
    uint64_t count;
};

static void report(const char *phase, const struct state *state)
{
    struct utsname kernel;
    char boot_id[37];
    FILE *boot = fopen("/proc/sys/kernel/random/boot_id", "re");

    if (!boot || fscanf(boot, "%36s", boot_id) != 1 || uname(&kernel) < 0)
        err(1, "reading the running kernel identity");
    fclose(boot);
    printf("CPS_VM_HANDOVER: %s %016" PRIx64 " %s %s %" PRIu64 "\n",
           phase, state->nonce, kernel.release, boot_id, state->count);
    if (fflush(stdout) == EOF)
        err(1, "reporting workload state");
}

static void prepare(struct state *state)
{
    int fd = memfd_create("cps-vm-workload", 0);

    if (fd < 0 || getrandom(&state->nonce, sizeof(state->nonce), GRND_INSECURE) != sizeof(state->nonce)
        || write(fd, state, sizeof(*state)) != sizeof(*state) || dup2(fd, 3) < 0)
        err(1, "creating workload state in memory");
    if (fd != 3)
        close(fd);
    report("before", state);
}

int main(int argc, char **argv)
{
    struct state state = { .count = 41 };

    if (argc >= 3 && strcmp(argv[1], "send") == 0) {
        prepare(&state);
        execv(argv[2], &argv[2]);
        err(1, "starting kernel handover");
    }
    if (argc == 2 && strcmp(argv[1], "receive") == 0) {
        const char *fd = getenv("CPS_HANDOVER_FD");

        if (!fd || pread(atoi(fd), &state, sizeof(state), 0) != sizeof(state))
            err(1, "reading restored workload memory");
        state.count++;
        report("after", &state);
        if (reboot(RB_POWER_OFF) < 0)
            err(1, "shutting down the VM");
    }
    if (argc == 3 && strcmp(argv[1], "fdstore") == 0) {
        const char *fds = getenv("LISTEN_FDS");

        if (fds) {
            if (strcmp(fds, "1") != 0 || pread(3, &state, sizeof(state), 0) != sizeof(state))
                errx(1, "expected one restored memfd");
            state.count++;
            report("after", &state);
            return 0;
        }
        prepare(&state);
        execl(argv[2], argv[2], "--fd=3", "--fdname=cps-vm-workload", "FDSTORE=1", (char *) NULL);
        err(1, "storing the workload memfd with systemd");
    }
    errx(1, "expected send, receive or fdstore");
}
