/* Real X server integration for the production native image-transfer owner.
   No codec/authority stand-in: these tests isolate the X11 boundary itself. */
#define _POSIX_C_SOURCE 200809L
#include "../src/x11_image.h"
#include <X11/Xutil.h>
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <dirent.h>
#include <errno.h>
#include <stddef.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/syscall.h>
static uint64_t micros(void) {
    struct timespec now;
    assert(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
    return (uint64_t)now.tv_sec*1000000 + (uint64_t)now.tv_nsec/1000;
}
static int compare(const void *a, const void *b) {
    uint64_t x=*(const uint64_t *)a, y=*(const uint64_t *)b;
    return (x>y)-(x<y);
}
static int descriptors(void) {
    DIR *dir=opendir("/proc/self/fd"); assert(dir);
    int n=0;
    while (readdir(dir)) n++;
    closedir(dir);
    return n;
}
/* Force a real kernel reservation refusal in this test process only. No
   allocation hook or altered image implementation is linked into the test. */
static void refuse_reservation(int error) {
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,nr)),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,__NR_fallocate,0,1),
        BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ERRNO|(unsigned int)error),
        BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ALLOW)
    };
    struct sock_fprog program={sizeof(filter)/sizeof(filter[0]),filter};
    assert(prctl(PR_SET_NO_NEW_PRIVS,1UL,0UL,0UL,0UL)==0);
    assert(prctl(PR_SET_SECCOMP,SECCOMP_MODE_FILTER,&program)==0);
}
static void allocation_refused(Display *d, Window root, Visual *visual, GC gc, int error) {
    const int w=64,h=64;
    uint8_t pixels[64*64*4];
    FrXImageTransfer *t=NULL;
    FrXImageStats stats;
    /* Prove SHM is available before injecting failure; a missing extension
       must not let this regression pass without exercising allocation. */
    assert(fr_ximage_new(d,visual,24,w,h,1,&t)==0);
    fr_ximage_stats(t,&stats);
    assert(stats.path==1 && stats.fallback==0);
    fr_ximage_free(t);
    int original_fds=descriptors();
    refuse_reservation(error);
    for (int pass=1;pass<=4;pass++) {
        XSetForeground(d,gc,0x2468ac); XFillRectangle(d,root,gc,0,0,w,h); XSync(d,False);
        assert(fr_ximage_new(d,visual,24,w,h,1,&t)==0);
        fr_ximage_stats(t,&stats);
        assert(stats.path==2 && stats.fallback==3 && stats.retained_bytes==0);
        assert(fr_ximage_capture(t,root,0,0,pixels,sizeof(pixels))==0);
        for (size_t i=0;i<sizeof(pixels);i+=4)
            assert(pixels[i]==0xac && pixels[i+1]==0x68 && pixels[i+2]==0x24 && pixels[i+3]==255);
        fr_ximage_free(t);
        assert(fr_ximage_new(d,visual,24,w,h,0,&t)==0);
        fr_ximage_stats(t,&stats);
        assert(stats.path==2 && stats.fallback==3 && stats.retained_bytes==sizeof(pixels));
        memset(pixels,pass*37,sizeof(pixels));
        assert(fr_ximage_store(t,pixels,sizeof(pixels))==0);
        assert(fr_ximage_draw(t,root,gc)==0);
        XImage *reference=XGetImage(d,root,0,0,w,h,AllPlanes,ZPixmap); assert(reference);
        unsigned long expected=(unsigned long)(pass*37)*0x010101;
        for (int y=0;y<h;y++) for (int x=0;x<w;x++)
            assert(XGetPixel(reference,x,y)==expected);
        XDestroyImage(reference);
        fr_ximage_free(t);
        assert(descriptors()==original_fds);
    }
    printf("{\"operation\":\"reservation_refused\",\"errno\":%d,\"capture_and_present_cycles\":4,\"path\":\"socket\",\"fallback\":\"shared_allocation_unavailable\"}\n",error);
}
static void presentation(Display *d, Window root, Visual *visual, GC gc, int shared) {
    const int w=1920,h=1080;
    const size_t len=(size_t)w*h*4;
    FrXImageTransfer *t=NULL;
    assert(fr_ximage_new(d,visual,24,w,h,0,&t)==0);
    assert(!fr_ximage_ready(t));
    assert(fr_ximage_draw(t,root,gc)==-1);
    uint8_t *pixels=malloc(len); assert(pixels);
    FrXImageStats stats;
    for (int pass=1;pass<=3;pass++) {
        memset(pixels,pass*37,len);
        assert(fr_ximage_store(t,pixels,len-1)==-1);
        assert(fr_ximage_capture(t,root,0,0,pixels,len)==-1);
        assert(fr_ximage_store(t,pixels,len)==0);
        assert(fr_ximage_ready(t));
        assert(fr_ximage_draw(t,root,gc)==0);
        XImage *reference=XGetImage(d,root,0,0,w,h,AllPlanes,ZPixmap); assert(reference);
        unsigned long expected=(unsigned long)(pass*37)*0x010101;
        for (int y=0;y<h;y++) for (int x=0;x<w;x++)
            assert(XGetPixel(reference,x,y)==expected);
        XDestroyImage(reference);
    }
    uint64_t samples[300];
    for (int i=0;i<300;i++) {
        uint64_t begin=micros();
        assert(fr_ximage_store(t,pixels,len)==0);
        assert(fr_ximage_draw(t,root,gc)==0);
        samples[i]=micros()-begin;
    }
    fr_ximage_stats(t,&stats);
    assert(stats.path==(shared?1u:2u) && stats.retained_bytes==len);
    assert(stats.images==303 && stats.copied_bytes==303*len);
    assert(stats.socket_pixel_bytes==(shared?0:303*len));
    XSetForeground(d,gc,0); XFillRectangle(d,root,gc,0,0,w,h); XSync(d,False);
    assert(fr_ximage_draw(t,root,gc)==0); /* Idle replay: no new pixel copy. */
    fr_ximage_stats(t,&stats);
    assert(stats.images==304 && stats.copied_bytes==303*len);
    XImage *reference=XGetImage(d,root,0,0,w,h,AllPlanes,ZPixmap); assert(reference);
    assert(XGetPixel(reference,w-1,h-1)==0x6f6f6f);
    XDestroyImage(reference);
    qsort(samples,300,sizeof(samples[0]),compare);
    printf("{\"operation\":\"present\",\"path\":\"%s\",\"frames\":300,\"width\":1920,\"height\":1080,\"p50_us\":%llu,\"p95_us\":%llu,\"copy_bytes_per_frame\":%zu,\"socket_pixel_bytes_per_frame\":%zu}\n",
           shared?"shared":"socket",(unsigned long long)samples[150],
           (unsigned long long)samples[285],len,shared?0:len);
    if (shared) {
        assert(fr_ximage_draw(t,0,gc)==-6);
        assert(!fr_ximage_ready(t));
        assert(fr_ximage_store(t,pixels,len)==-1);
    }
    fr_ximage_free(t);
    if (shared) {
        struct rlimit old, limited;
        assert(getrlimit(RLIMIT_NOFILE,&old)==0);
        limited=old; limited.rlim_cur=(rlim_t)ConnectionNumber(d)+1;
        assert(setrlimit(RLIMIT_NOFILE,&limited)==0);
        int opened=fr_ximage_new(d,visual,24,w,h,0,&t);
        assert(setrlimit(RLIMIT_NOFILE,&old)==0);
        assert(opened==0);
        fr_ximage_stats(t,&stats);
        assert(stats.path==2 && stats.fallback==3 && stats.retained_bytes==len);
        assert(fr_ximage_store(t,pixels,len)==0 && fr_ximage_draw(t,root,gc)==0);
        fr_ximage_free(t);
    }
    free(pixels);
}
int main(int argc, char **argv) {
    assert(argc==2);
    int allocation_error=!strcmp(argv[1],"enospc")?ENOSPC:
        !strcmp(argv[1],"enomem")?ENOMEM:!strcmp(argv[1],"unsupported")?EOPNOTSUPP:0;
    assert(allocation_error || !strcmp(argv[1],"shared") || !strcmp(argv[1],"socket"));
    int shared=strcmp(argv[1],"shared")==0;
    Display *d=XOpenDisplay(NULL); assert(d);
    int screen=DefaultScreen(d);
    Window root=RootWindow(d,screen);
    Visual *visual=DefaultVisual(d,screen);
    GC gc=XCreateGC(d,root,0,NULL); assert(gc);
    if (allocation_error) {
        allocation_refused(d,root,visual,gc,allocation_error);
        XFreeGC(d,gc); XCloseDisplay(d);
        return 0;
    }
    const int w=1920,h=1080;
    XSetForeground(d,gc,0x2468ac); XFillRectangle(d,root,gc,0,0,w,h);
    XSetForeground(d,gc,0xeca864); XFillRectangle(d,root,gc,100,40,64,64);
    XSync(d,False);
    FrXImageTransfer *t=NULL;
    assert(fr_ximage_new(d,visual,24,w,h,1,&t)==0 && t);
    FrXImageStats stats;
    fr_ximage_stats(t,&stats);
    assert(stats.path==(shared?1u:2u));
    assert(stats.fallback==(shared?0u:1u));
    assert(stats.images==0 && stats.copied_bytes==0);
    size_t len=(size_t)w*h*4;
    assert(stats.retained_bytes==(shared?len:0));
    uint8_t *pixels=malloc(len+16); assert(pixels);
    memset(pixels+len,0xad,16);
    assert(fr_ximage_capture(t,root,0,0,pixels,len-1)==-1);
    assert(fr_ximage_capture(t,root,-1,0,pixels,len)==-1);
    assert(fr_ximage_capture(t,root,0,0,pixels,len)==0);
    XImage *reference=XGetImage(d,root,0,0,w,h,AllPlanes,ZPixmap); assert(reference);
    for (int y=0;y<h;y++) for (int x=0;x<w;x++) {
        size_t i=((size_t)y*w+x)*4;
        unsigned long p=XGetPixel(reference,x,y);
        assert(pixels[i]==(p&255) && pixels[i+1]==((p>>8)&255) && pixels[i+2]==((p>>16)&255));
        assert(pixels[i+3]==255);
    }
    XDestroyImage(reference);
    for (size_t i=len;i<len+16;i++) assert(pixels[i]==0xad);
    FrXImageTransfer *rect=NULL;
    assert(fr_ximage_new(d,visual,24,64,64,1,&rect)==0);
    assert(fr_ximage_capture(rect,root,100,40,pixels,64*64*4)==0);
    for (size_t i=0;i<64*64*4;i+=4) {
        assert(pixels[i]==0x64 && pixels[i+1]==0xa8 && pixels[i+2]==0xec && pixels[i+3]==255);
    }
    fr_ximage_free(rect);
    FrXImageTransfer *invalid=NULL;
    for (int v=0;v<3;v++) {
        assert(fr_ximage_new(d,visual,24,v==0?0:v==1?8194:8192,8192,1,&invalid)==-1);
        assert(!invalid);
    }
    uint64_t samples[300];
    for (int i=0;i<300;i++) {
        uint64_t begin=micros();
        assert(fr_ximage_capture(t,root,0,0,pixels,len)==0);
        samples[i]=micros()-begin;
    }
    fr_ximage_stats(t,&stats);
    assert(stats.images==301 && stats.copied_bytes==301*len);
    assert(stats.socket_pixel_bytes==(shared?0:301*len));
    assert(stats.retained_bytes==(shared?len:0));
    qsort(samples,300,sizeof(samples[0]),compare);
    printf("{\"path\":\"%s\",\"frames\":300,\"width\":1920,\"height\":1080,\"p50_us\":%llu,\"p95_us\":%llu,\"copy_bytes_per_frame\":%zu,\"socket_pixel_bytes_per_frame\":%zu,\"retained_bytes\":%llu}\n",
           argv[1],(unsigned long long)samples[150],(unsigned long long)samples[285],len,shared?0:len,
           (unsigned long long)stats.retained_bytes);
    fr_ximage_free(t);
    int original_fds=descriptors();
    if (shared) {
        /* A real descriptor-allocation failure is reported, not retried or
           confused with a missing server capability. Existing X socket works. */
        struct rlimit old, limited;
        assert(getrlimit(RLIMIT_NOFILE,&old)==0);
        limited=old; limited.rlim_cur=(rlim_t)ConnectionNumber(d)+1;
        assert(setrlimit(RLIMIT_NOFILE,&limited)==0);
        int opened=fr_ximage_new(d,visual,24,64,64,1,&t);
        assert(setrlimit(RLIMIT_NOFILE,&old)==0);
        assert(opened==0);
        fr_ximage_stats(t,&stats);
        assert(stats.path==2 && stats.fallback==3);
        assert(fr_ximage_capture(t,root,100,40,pixels,64*64*4)==0);
        fr_ximage_free(t);
        /* Checked errors affect only their original transfer, with no Xlib
           global error-handler replacement or permissive fallback after use. */
        assert(fr_ximage_new(d,visual,24,64,64,1,&t)==0);
        assert(fr_ximage_capture(t,0,0,0,pixels,64*64*4)==-6);
        assert(fr_ximage_capture(t,root,0,0,pixels,64*64*4)==-1);
        fr_ximage_free(t);
    }
    /* Multiple close/reopen cycles cannot leak client descriptors. */
    for (int i=0;i<20;i++) {
        assert(fr_ximage_new(d,visual,24,64,64,1,&t)==0);
        assert(fr_ximage_capture(t,root,100,40,pixels,64*64*4)==0);
        fr_ximage_free(t);
    }
    assert(descriptors()==original_fds);
    presentation(d,root,visual,gc,shared);
    assert(descriptors()==original_fds);
    free(pixels); XFreeGC(d,gc); XCloseDisplay(d);
    return 0;
}
