#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <linux/audit.h>
#include <linux/capability.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static void check(int ok, const char *operation)
{
    if (!ok) {
        perror(operation);
        exit(1);
    }
}

static void drop_ids(void)
{
    check(setgroups(0, NULL) == 0, "setgroups");
    check(setgid(65534) == 0, "setgid");
    check(setuid(65534) == 0, "setuid");
}

int main(int argc, char **argv)
{
    if (argc == 5 && strcmp(argv[1], "launch") == 0) {
        drop_ids();
        execl(argv[2], argv[2], "expect", argv[3], argv[4], NULL);
        check(0, "exec probe");
    }
    if (argc == 4 && strcmp(argv[1], "expect") == 0) {
        return !(getuid() == 65534 && getgid() == 65534 &&
                 geteuid() == (uid_t) strtoul(argv[2], NULL, 10) &&
                 getegid() == (gid_t) strtoul(argv[3], NULL, 10));
    }
    if (argc >= 3 && strcmp(argv[1], "no-fsetid") == 0) {
        check(setgroups(0, NULL) == 0, "clear groups");
        check(setgid(65534) == 0, "change group");
        check(prctl(PR_CAPBSET_DROP, CAP_FSETID, 0, 0, 0) == 0, "drop CAP_FSETID");
        execv(argv[2], argv + 2);
        check(0, "exec without CAP_FSETID");
    }
    if (argc >= 3 && strcmp(argv[1], "deny-link") == 0) {
        struct sock_filter instructions[] = {
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_linkat, 0, 1),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        };
        struct sock_fprog filter = {
            .len = sizeof(instructions) / sizeof(instructions[0]),
            .filter = instructions,
        };
        check(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0, "no_new_privs");
        check(prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &filter) == 0, "deny linkat");
        execv(argv[2], argv + 2);
        check(0, "exec with denied linkat");
    }
    if (argc == 3 && strcmp(argv[1], "sticky") == 0) {
        drop_ids();
        errno = 0;
        return !(unlink(argv[2]) == -1 && errno == EPERM);
    }
    if (argc == 3 && strcmp(argv[1], "inherit") == 0) {
        struct stat info;
        drop_ids();
        int fd = open(argv[2], O_WRONLY | O_CREAT | O_EXCL, 0600);
        check(fd >= 0, "create inherited group file");
        check(fstat(fd, &info) == 0, "stat inherited group file");
        check(close(fd) == 0, "close inherited group file");
        return !(info.st_uid == 65534 && info.st_gid == 0);
    }
    fprintf(stderr, "invalid probe arguments\n");
    return 2;
}
