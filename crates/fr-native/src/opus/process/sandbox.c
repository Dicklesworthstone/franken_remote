/* Linux x86-64 audio decoder confinement. No X11/Pulse/network descriptor,
 * file access, executable mappings, process creation or authority reaches it.
 * The selected executable, native package and kernel remain trusted. */
#define _GNU_SOURCE
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/resource.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#if defined(__x86_64__) && !defined(__ILP32__)
#define DENY BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ERRNO|EPERM)
#define ALLOW BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ALLOW)
#define ARG(n) BPF_STMT(BPF_LD|BPF_W|BPF_ABS, offsetof(struct seccomp_data,args[n]))
#define BEGIN(n,k) BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(n),0,(k))
#define SIMPLE(n) BEGIN((n),1),ALLOW
#define FD(n,a,b) BEGIN((n),5),ARG(0), \
 BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(a),2,0), \
 BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(b),1,0),DENY,ALLOW
int fr_opus_sandbox_enter(void) {
    for (int fd=0; fd<2; ++fd) {
        int type=0; socklen_t len=sizeof(type);
        struct ucred peer; socklen_t peer_len=sizeof(peer);
        if (getsockopt(fd,SOL_SOCKET,SO_TYPE,&type,&len) || type!=SOCK_STREAM ||
            getsockopt(fd,SOL_SOCKET,SO_PEERCRED,&peer,&peer_len) ||
            peer.uid!=getuid() || peer.pid!=getppid()) return 0;
    }
    struct rlimit memory={256UL*1024UL*1024UL,256UL*1024UL*1024UL};
    struct rlimit zero={0,0};
    if (setrlimit(RLIMIT_AS,&memory) || setrlimit(RLIMIT_CORE,&zero) ||
        setrlimit(RLIMIT_FSIZE,&zero) || syscall(__NR_close_range,3U,~0U,0U)) return 0;
    struct sock_filter filter[]={
        BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,arch)),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,AUDIT_ARCH_X86_64,1,0),
        BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,nr)),
        FD(__NR_read,0,0), FD(__NR_readv,0,0),
        FD(__NR_write,1,2), FD(__NR_writev,1,2),
        FD(__NR_fstat,0,1),
        BEGIN(__NR_mmap,7),ARG(2),
        BPF_JUMP(BPF_JMP|BPF_JSET|BPF_K,PROT_EXEC,0,1),DENY,
        ARG(3),BPF_JUMP(BPF_JMP|BPF_JSET|BPF_K,MAP_ANONYMOUS,1,0),DENY,ALLOW,
        BEGIN(__NR_mprotect,4),ARG(2),
        BPF_JUMP(BPF_JMP|BPF_JSET|BPF_K,PROT_EXEC,0,1),DENY,ALLOW,
        SIMPLE(__NR_close),SIMPLE(__NR_brk),SIMPLE(__NR_munmap),
        SIMPLE(__NR_mremap),SIMPLE(__NR_madvise),SIMPLE(__NR_futex),
        SIMPLE(__NR_sched_yield),SIMPLE(__NR_clock_gettime),SIMPLE(__NR_gettimeofday),
        SIMPLE(__NR_rt_sigaction),SIMPLE(__NR_rt_sigprocmask),SIMPLE(__NR_rt_sigreturn),
        SIMPLE(__NR_sigaltstack),SIMPLE(__NR_getpid),SIMPLE(__NR_gettid),
        SIMPLE(__NR_getrandom),SIMPLE(__NR_sched_getaffinity),
        SIMPLE(__NR_exit),SIMPLE(__NR_exit_group),DENY
    };
    struct sock_fprog program={sizeof(filter)/sizeof(filter[0]),filter};
    return prctl(PR_SET_NO_NEW_PRIVS,1UL,0UL,0UL,0UL)==0 &&
        syscall(__NR_seccomp,SECCOMP_SET_MODE_FILTER,SECCOMP_FILTER_FLAG_TSYNC,&program)==0;
}
#else
int fr_opus_sandbox_enter(void) { return 0; }
#endif
