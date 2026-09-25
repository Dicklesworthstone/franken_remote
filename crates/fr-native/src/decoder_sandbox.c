/* CPU decoder syscall confinement; included only by the named native bridge.
 * The existing X server remains a broad trust boundary, NOT input isolation.
 * Linux UAPI: userspace-api/seccomp_filter and userspace-api/no_new_privs. */
#include <stddef.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
/* The bridge otherwise compiles as strict C11. */
extern long syscall(long number, ...);
#if defined(__x86_64__) && !defined(__ILP32__)
#define FR_SC_DENY BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ERRNO|EPERM)
#define FR_SC_ALLOW BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ALLOW)
#define FR_SC_ARG(n) BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,args[n]))
#define FR_SC_BEGIN(n,count) BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(n),0,(count))
#define FR_SC_SIMPLE(n) FR_SC_BEGIN((n),1),FR_SC_ALLOW
/* Each matching path terminates. Nonmatches retain the syscall number in A.
 * Linux descriptor arguments are ints: low-word comparison matches the kernel. */
#define FR_SC_FD3(n,a,b,c) \
 FR_SC_BEGIN((n),6),FR_SC_ARG(0),\
 BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(a),3,0),\
 BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(b),2,0),\
 BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(c),1,0),FR_SC_DENY,FR_SC_ALLOW
#define FR_SC_FD4(n,a,b,c,d) \
 FR_SC_BEGIN((n),7),FR_SC_ARG(0),\
 BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(a),4,0),\
 BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(b),3,0),\
 BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(c),2,0),\
 BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,(d),1,0),FR_SC_DENY,FR_SC_ALLOW
static int fr_decoder_sandbox_enter(int input,int output,int diagnostic,int xfd) {
    if (input<0 || output<0 || diagnostic<0 || xfd<0 ||
        input==output || input==diagnostic || input==xfd ||
        output==diagnostic || output==xfd || diagnostic==xfd) return 0;
    int in_flags=fcntl(input,F_GETFL),out_flags=fcntl(output,F_GETFL);
    int err_flags=fcntl(diagnostic,F_GETFL),x_flags=fcntl(xfd,F_GETFL);
    if (in_flags<0 || out_flags<0 || err_flags<0 || x_flags<0 ||
        (in_flags&O_ACCMODE)==O_WRONLY || (out_flags&O_ACCMODE)==O_RDONLY ||
        (err_flags&O_ACCMODE)==O_RDONLY || (x_flags&O_ACCMODE)!=O_RDWR) return 0;
    unsigned int self=(unsigned int)getpid();
    struct sock_filter filter[]={
        BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,arch)),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,AUDIT_ARCH_X86_64,1,0),
        BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,nr)),
        /* Unknown and x32 alias numbers cannot match this native allowlist. */
        FR_SC_FD3(__NR_read,input,xfd,xfd),
        FR_SC_FD3(__NR_readv,input,xfd,xfd),
        FR_SC_FD3(__NR_write,output,diagnostic,xfd),
        FR_SC_FD3(__NR_writev,output,diagnostic,xfd),
        FR_SC_FD3(__NR_recvmsg,xfd,xfd,xfd),
        FR_SC_FD3(__NR_recvfrom,xfd,xfd,xfd),
        FR_SC_FD3(__NR_sendmsg,xfd,xfd,xfd),
        FR_SC_FD3(__NR_sendto,xfd,xfd,xfd),
        FR_SC_FD3(__NR_shutdown,xfd,xfd,xfd),
        FR_SC_FD4(__NR_fstat,input,output,diagnostic,xfd),
        /* No general device ioctls; only pending-byte queries on original X11. */
        FR_SC_BEGIN(__NR_ioctl,7),FR_SC_ARG(0),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,xfd,1,0),FR_SC_DENY,
        FR_SC_ARG(1),BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,FIONREAD,1,0),FR_SC_DENY,FR_SC_ALLOW,
        /* No duplication, ownership signals, locks or leases. */
        FR_SC_BEGIN(__NR_fcntl,12),FR_SC_ARG(0),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,input,4,0),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,output,3,0),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,diagnostic,2,0),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,xfd,1,0),FR_SC_DENY,
        FR_SC_ARG(1),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,F_GETFD,3,0),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,F_GETFL,2,0),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,F_SETFL,1,0),FR_SC_DENY,FR_SC_ALLOW,
        /* Existing native code remains executable; no file mappings or new
         * executable allocations/permissions are admitted by this profile. */
        FR_SC_BEGIN(__NR_mmap,7),FR_SC_ARG(2),
        BPF_JUMP(BPF_JMP|BPF_JSET|BPF_K,PROT_EXEC,0,1),FR_SC_DENY,
        FR_SC_ARG(3),BPF_JUMP(BPF_JMP|BPF_JSET|BPF_K,MAP_ANONYMOUS,1,0),FR_SC_DENY,FR_SC_ALLOW,
        FR_SC_BEGIN(__NR_mprotect,4),FR_SC_ARG(2),
        BPF_JUMP(BPF_JMP|BPF_JSET|BPF_K,PROT_EXEC,0,1),FR_SC_DENY,FR_SC_ALLOW,
        FR_SC_BEGIN(__NR_tgkill,4),FR_SC_ARG(0),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,self,1,0),FR_SC_DENY,FR_SC_ALLOW,
        FR_SC_SIMPLE(__NR_close),FR_SC_SIMPLE(__NR_brk),
        FR_SC_SIMPLE(__NR_munmap),FR_SC_SIMPLE(__NR_mremap),FR_SC_SIMPLE(__NR_madvise),
        FR_SC_SIMPLE(__NR_futex),FR_SC_SIMPLE(__NR_sched_yield),
        FR_SC_SIMPLE(__NR_poll),FR_SC_SIMPLE(__NR_ppoll),
        FR_SC_SIMPLE(__NR_select),FR_SC_SIMPLE(__NR_pselect6),
        FR_SC_SIMPLE(__NR_clock_gettime),FR_SC_SIMPLE(__NR_gettimeofday),
        FR_SC_SIMPLE(__NR_nanosleep),FR_SC_SIMPLE(__NR_clock_nanosleep),
        FR_SC_SIMPLE(__NR_rt_sigaction),FR_SC_SIMPLE(__NR_rt_sigprocmask),
        FR_SC_SIMPLE(__NR_rt_sigreturn),FR_SC_SIMPLE(__NR_sigaltstack),
        FR_SC_SIMPLE(__NR_getpid),FR_SC_SIMPLE(__NR_gettid),
        FR_SC_SIMPLE(__NR_getuid),FR_SC_SIMPLE(__NR_geteuid),
        FR_SC_SIMPLE(__NR_getgid),FR_SC_SIMPLE(__NR_getegid),
        FR_SC_SIMPLE(__NR_getrandom),FR_SC_SIMPLE(__NR_sched_getaffinity),
        FR_SC_SIMPLE(__NR_uname),FR_SC_SIMPLE(__NR_getrusage),
        FR_SC_SIMPLE(__NR_exit),FR_SC_SIMPLE(__NR_exit_group),
        FR_SC_DENY
    };
    struct sock_fprog program={sizeof(filter)/sizeof(filter[0]),filter};
    if (prctl(PR_SET_NO_NEW_PRIVS,1UL,0UL,0UL,0UL)!=0) return 0;
    /* Exact zero is essential: a positive TSYNC result is an unsynchronized TID.
     * Even a partial setup failure is terminal; callers may never fall back. */
    return syscall(__NR_seccomp,SECCOMP_SET_MODE_FILTER,
                   SECCOMP_FILTER_FLAG_TSYNC,&program)==0;
}
#undef FR_SC_DENY
#undef FR_SC_ALLOW
#undef FR_SC_ARG
#undef FR_SC_BEGIN
#undef FR_SC_SIMPLE
#undef FR_SC_FD3
#undef FR_SC_FD4
#else
static int fr_decoder_sandbox_enter(int input,int output,int diagnostic,int xfd) {
    (void)input; (void)output; (void)diagnostic; (void)xfd;
    return 0; /* Unqualified architecture: no unrestricted fallback. */
}
#endif
int fr_x11_confine_decoder(FrX11 *x,int input,int output,int diagnostic) {
    if (!x || !x->display || !x->presenter || x->invalid) return 0;
    if (fr_x11_geometry(x)!=FR_OK) return 0;
    /* Allocate/attach exactly one presentation buffer BEFORE the existing
       sandbox forbids new descriptors and file-backed mappings. No extra
       syscall is allowed after confinement; an uninitialized image is never
       painted. The buffer remains inside this independently supervised child. */
    if (fr_x11_front(x,(size_t)x->w*(size_t)x->h*4)!=FR_OK) return 0;
    return fr_decoder_sandbox_enter(input,output,diagnostic,ConnectionNumber(x->display));
}
