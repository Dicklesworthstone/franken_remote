/* Real production bridge, software HEVC and the unchanged seccomp filter.
 * This is native CPU evidence, not transport, scanout or hardware evidence.
 * scripts/test_native_decoder.sh also runs the optional paired 1080p timing rows. */
#define _GNU_SOURCE
#include "../src/bridge.c"
#include <assert.h>
#include <sched.h>
#include <stdatomic.h>
#include <time.h>
#include <sys/wait.h>
#ifdef NDEBUG
#error "Native decoder assertions must remain enabled"
#endif
/* glibc's assertion formatter uses %n and may read /proc/self/maps to prove
 * its format is immutable. The application sandbox intentionally denies that
 * open, so use a fixed bounded diagnostic even for a confined failing test. */
#undef assert
#define assert(condition) do { \
    if (!(condition)) { \
        fprintf(stderr,"native_check_failed line=%d expression=%s\n",__LINE__,#condition); \
        fflush(stderr); abort(); \
    } \
} while (0)

typedef struct { uint8_t *bytes; size_t length; int64_t id; } Unit;
enum { MAX_UNITS=65, CONFIG_CAPACITY=12326, PIXEL_CANARY=64 };

static uint64_t monotonic_ns(void) {
    struct timespec t;
    assert(clock_gettime(CLOCK_MONOTONIC,&t)==0);
    return (uint64_t)t.tv_sec*1000000000+(uint64_t)t.tv_nsec;
}
static int compare_time(const void *a,const void *b) {
    uint64_t aa=*(const uint64_t *)a,bb=*(const uint64_t *)b;
    return aa>bb?1:aa<bb?-1:0;
}
static size_t start_code(const uint8_t *p,size_t n,size_t at,size_t *length) {
    for (size_t i=at;i+3<=n;i++) {
        if (p[i]==0 && p[i+1]==0 && p[i+2]==1) { *length=3; return i; }
        if (i+4<=n && p[i]==0 && p[i+1]==0 && p[i+2]==0 && p[i+3]==1) {
            *length=4; return i;
        }
    }
    *length=0; return n;
}
static void be32(uint8_t *p,size_t n) {
    p[0]=(uint8_t)(n>>24); p[1]=(uint8_t)(n>>16);
    p[2]=(uint8_t)(n>>8); p[3]=(uint8_t)n;
}
/* Convert this test encoder's Annex B units to the production four-byte
 * framing, collecting its exact VPS/SPS/PPS for native configuration. Rust's
 * parameter-set/generation guard is covered separately in prepared.rs. */
static Unit framed(const uint8_t *bytes,size_t length,int64_t id,
                   uint8_t *configuration,size_t *configuration_len) {
    size_t capacity=length*2+4;
    Unit u={malloc(capacity),0,id};
    assert(u.bytes);
    size_t prefix=0,next_prefix=0,at=start_code(bytes,length,0,&prefix);
    int parameters=0;
    assert(at==0);
    while (at<length) {
        size_t nal=at+prefix,next=start_code(bytes,length,nal,&next_prefix),len=next-nal;
        while (len>2 && bytes[nal+len-1]==0) len--;
        assert(len>=2 && len<16777216 && u.length+4+len<=capacity);
        unsigned kind=(bytes[nal]>>1)&63;
        if (configuration && kind>=32 && kind<=34) {
            size_t off=*configuration_len;
            assert(len<=UINT16_MAX && off+5+len<=CONFIG_CAPACITY);
            configuration[off]=(uint8_t)(0x80|kind);
            configuration[off+1]=0; configuration[off+2]=1;
            configuration[off+3]=(uint8_t)(len>>8); configuration[off+4]=(uint8_t)len;
            memcpy(configuration+off+5,bytes+nal,len);
            *configuration_len+=len+5; parameters++;
        }
        be32(u.bytes+u.length,len); u.length+=4;
        memcpy(u.bytes+u.length,bytes+nal,len); u.length+=len;
        at=next; prefix=next_prefix;
    }
    assert(!configuration || parameters==3);
    return u;
}
static uint64_t decode(FrDecoder *d,Unit u,uint8_t *out,size_t size) {
    int64_t pts=-1;
    uint64_t began=monotonic_ns();
    assert(fr_decoder_send(d,u.bytes,u.length,u.id)==FR_OK);
    assert(fr_decoder_receive(d,out,size,&pts)==FR_OK && pts==u.id);
    return monotonic_ns()-began;
}
static void output_canaries(const uint8_t *pixels,size_t size,uint8_t sentinel) {
    const uint8_t *before=pixels-PIXEL_CANARY;
    for (size_t i=0;i<PIXEL_CANARY;i++) {
        assert(before[i]==sentinel);
        assert(pixels[size+i]==sentinel);
    }
    for (size_t i=3;i<size;i+=4) assert(pixels[i]==255);
}
static FrDecoder *prepared(int w,int h,int coded_w,int coded_h) {
    FrDecoder *d=NULL;
    assert(fr_decoder_prepare(w,h,coded_w,coded_h,&d)==FR_OK);
    assert(d && d->ctx->thread_count==4 && d->ctx->active_thread_type==FF_THREAD_SLICE);
    assert(d->ctx->extradata==NULL && d->ctx->extradata_size==0 && d->configured==0);
    return d;
}
static void configure(FrDecoder *d,const uint8_t *configuration,size_t length) {
    assert(fr_decoder_configure(d,configuration,length)==FR_OK);
    int64_t pts=-1;
    assert(fr_decoder_receive_reference(d,&pts)==FR_AGAIN && pts==-1);
    // A second configure cannot replace the original decoder state.
    assert(fr_decoder_configure(d,configuration,length)==FR_INVALID);
}

typedef struct { _Atomic unsigned seen; } Probe;
static int denied_on_worker(AVCodecContext *codec,void *arg,int job,int thread) {
    (void)codec; (void)job;
    Probe *probe=arg;
    assert(thread>=0 && thread<4);
    errno=0; int fd=open("/etc/passwd",O_RDONLY);
    assert(fd==-1 && errno==EPERM);
    errno=0; fd=socket(AF_INET,SOCK_STREAM,0);
    assert(fd==-1 && errno==EPERM);
    // Invalid clone3 arguments never create a process even in a planted
    // unrestricted run; the filter must refuse with EPERM, not EINVAL/ENOSYS.
    errno=0; long spawned=syscall(__NR_clone3,NULL,0);
    assert(spawned==-1 && errno==EPERM);
    atomic_fetch_or(&probe->seen,1U<<(unsigned)thread);
    uint64_t deadline=monotonic_ns()+2000000000;
    while (atomic_load(&probe->seen)!=15) {
        assert(monotonic_ns()<deadline);
        sched_yield();
    }
    return 0;
}
static void confined(Unit *units,int count,const uint8_t *configuration,size_t length,
                     int w,int h,int coded_w,int coded_h,const uint8_t *expected) {
    pid_t child=fork();
    assert(child>=0);
    if (!child) {
        int input[2],x[2];
        assert(pipe(input)==0 && socketpair(AF_UNIX,SOCK_STREAM,0,x)==0);
        // Prove that the same file and network operations are permitted before
        // the filter; missing files or host policy cannot manufacture a pass.
        int file=open("/etc/passwd",O_RDONLY),network=socket(AF_INET,SOCK_STREAM,0);
        assert(file>=0 && network>=0); close(file); close(network);
        FrDecoder *d=prepared(w,h,coded_w,coded_h);
        int64_t pts=-1;
        assert(fr_decoder_send(d,units[0].bytes,units[0].length,0)==FR_INVALID);
        assert(fr_decoder_receive_reference(d,&pts)==FR_INVALID);
        assert(fr_decoder_sandbox_enter(input[0],STDOUT_FILENO,STDERR_FILENO,x[0]));
        Probe probe; atomic_init(&probe.seen,0);
        int returned[4]={-1,-1,-1,-1};
        assert(d->ctx->execute2(d->ctx,denied_on_worker,&probe,returned,4)==0);
        assert(atomic_load(&probe.seen)==15);
        for (int i=0;i<4;i++) assert(returned[i]==0);
        configure(d,configuration,length);
        size_t size=(size_t)w*h*4;
        uint8_t *allocation=malloc(size+2*PIXEL_CANARY); assert(allocation);
        memset(allocation,0x69,size+2*PIXEL_CANARY);
        uint8_t *out=allocation+PIXEL_CANARY;
        for (int i=0;i<count;i++) {
            if (i%3==0 && i<count-1) {
                assert(fr_decoder_send(d,units[i].bytes,units[i].length,units[i].id)==FR_OK);
                assert(fr_decoder_receive_reference(d,&pts)==FR_OK && pts==units[i].id);
            } else {
                (void)decode(d,units[i],out,size);
                output_canaries(out,size,0x69);
            }
        }
        assert(memcmp(out,expected,size)==0);
        fr_decoder_free(d);
        // The original constructor remains usable inside the unchanged filter.
        assert(fr_decoder_new(w,h,coded_w,coded_h,configuration,length,&d)==FR_OK);
        assert(d->ctx->thread_count==1);
        for (int i=0;i<count;i++) (void)decode(d,units[i],out,size);
        assert(memcmp(out,expected,size)==0);
        output_canaries(out,size,0x69);
        fr_decoder_free(d); free(allocation);
        printf("sandbox_decode=passed all_four_threads_confined=true legacy_single_thread=passed\n");
        fflush(stdout); _exit(0);
    }
    int status;
    assert(waitpid(child,&status,0)==child);
    assert(WIFEXITED(status) && WEXITSTATUS(status)==0);
}
static void refusals(const uint8_t *configuration,size_t length,Unit first,
                     int w,int h,int coded_w,int coded_h) {
    FrDecoder *d=NULL;
    assert(fr_decoder_prepare(8192,8192,8192,8192,&d)==FR_INVALID && d==NULL);
    assert(fr_decoder_prepare(w,h,coded_w-16,coded_h,&d)==FR_INVALID && d==NULL);
    d=prepared(w,h,coded_w,coded_h);
    // Corrupt the forbidden_zero_bit of the first real VPS. An empty hvcC
    // array is accepted by FFmpeg and rejected by Rust; it is not a native
    // parse-failure fixture. This complete record must fail inside the codec.
    uint8_t malformed[CONFIG_CAPACITY];
    assert(length>30 && (configuration[23]&63)==32);
    memcpy(malformed,configuration,length); malformed[28]|=0x80;
    assert(fr_decoder_configure(d,malformed,length)==FR_CODEC);
    assert(fr_decoder_configure(d,configuration,length)==FR_INVALID);
    fr_decoder_free(d);
    // Deferring parameter sets cannot bypass the original native pixel cap.
    d=prepared(16,16,16,16);
    int result=fr_decoder_configure(d,configuration,length);
    if (result==FR_OK) {
        result=fr_decoder_send(d,first.bytes,first.length,first.id);
        if (result==FR_OK) {
            uint8_t pixels[16*16*4]; int64_t pts=-1;
            result=fr_decoder_receive(d,pixels,sizeof(pixels),&pts);
        }
    }
    // FFmpeg 6.1 reports the refused frame-buffer allocation as ENOMEM;
    // other qualified ABIs may surface the geometry/codec error directly.
    assert(result==FR_MEMORY || result==FR_CODEC || result==FR_GEOMETRY);
    fr_decoder_free(d);
}
static void roundtrip(int w,int h,int count,int noise) {
    assert(count>5 && count<=MAX_UNITS);
    int coded_w=(w+15)&~15,coded_h=(h+15)&~15;
    size_t size=(size_t)w*h*4;
    Unit units[MAX_UNITS]={0};
    // hvcC carries the exact native parameter sets; Main, 8-bit 4:2:0 and a
    // four-byte NAL length match this explicitly selected test encoder.
    uint8_t configuration[CONFIG_CAPACITY]={1,1,0x60,0,0,0,0x90,0,0,0,0,0,120,
                                          0xf0,0,0xfc,0xfd,0xf8,0xf8,0,0,0x0f,3};
    size_t configuration_len=23;
    uint8_t *pixels=malloc(size),*one_allocation=malloc(size+2*PIXEL_CANARY),
            *four_allocation=malloc(size+2*PIXEL_CANARY);
    assert(pixels && one_allocation && four_allocation);
    uint8_t *one=one_allocation+PIXEL_CANARY,*four=four_allocation+PIXEL_CANARY;
    FrEncoder *encoder=NULL;
    assert(fr_encoder_new(2,w,h,30,8000000,60,0,&encoder)==FR_OK);
    uint32_t random=123;
    for (int i=0;i<count;i++) {
        for (int y=0;y<h;y++) for (int x=0;x<w;x++) {
            size_t at=((size_t)y*w+x)*4;
            if (noise) {
                random^=random<<13; random^=random>>17; random^=random<<5;
                pixels[at]=(uint8_t)random; pixels[at+1]=(uint8_t)(random>>8);
                pixels[at+2]=(uint8_t)(random>>16);
            } else {
                pixels[at]=(uint8_t)(((x/12+y/12+i)%7)*35);
                pixels[at+1]=(uint8_t)(x+i*7); pixels[at+2]=(uint8_t)(y+i*11);
            }
            pixels[at+3]=255;
        }
        assert(fr_encoder_send(encoder,pixels,size,i,i==0,0)==FR_OK);
        size_t length=0; int64_t pts=-1;
        assert(fr_encoder_peek(encoder,&length,&pts)==FR_OK && pts==i && length<=16777216);
        uint8_t *bytes=malloc(length); assert(bytes);
        assert(fr_encoder_take(encoder,bytes,length)==FR_OK);
        units[i]=framed(bytes,length,i,i==0?configuration:NULL,&configuration_len);
        free(bytes);
    }
    fr_encoder_free(encoder);
    FrDecoder *baseline=NULL;
    assert(fr_decoder_new(w,h,coded_w,coded_h,configuration,configuration_len,&baseline)==FR_OK);
    FrDecoder *parallel=prepared(w,h,coded_w,coded_h);
    configure(parallel,configuration,configuration_len);
    uint64_t single[MAX_UNITS],slices[MAX_UNITS];
    for (int i=0;i<count;i++) {
        uint64_t a,b;
        // Different recycled contents expose unwritten visible tails; leading
        // and trailing canaries detect writes beyond the exact Rust buffer.
        memset(one_allocation,0x5a,size+2*PIXEL_CANARY);
        memset(four_allocation,0xa5,size+2*PIXEL_CANARY);
        // Alternate order for paired measurements against the same packets.
        if (i%2) { b=decode(parallel,units[i],four,size); a=decode(baseline,units[i],one,size); }
        else { a=decode(baseline,units[i],one,size); b=decode(parallel,units[i],four,size); }
        output_canaries(one,size,0x5a); output_canaries(four,size,0xa5);
        assert(memcmp(one,four,size)==0);
        if (i>=5) { single[i-5]=a; slices[i-5]=b; }
    }
    size_t measured=(size_t)(count-5);
    qsort(single,measured,sizeof(*single),compare_time);
    qsort(slices,measured,sizeof(*slices),compare_time);
    printf("geometry=%dx%d content=%s samples=%zu pixels_identical=true single_p50_ms=%.3f single_p95_ms=%.3f four_slice_p50_ms=%.3f four_slice_p95_ms=%.3f\n",
           w,h,noise?"noise":"moving_desktop",measured,
           single[(measured-1)/2]/1e6,single[(measured*95-1)/100]/1e6,
           slices[(measured-1)/2]/1e6,slices[(measured*95-1)/100]/1e6);
    fflush(stdout);
    fr_decoder_free(baseline); fr_decoder_free(parallel);
    refusals(configuration,configuration_len,units[0],w,h,coded_w,coded_h);
    confined(units,count,configuration,configuration_len,w,h,coded_w,coded_h,one);
    for (int i=0;i<count;i++) free(units[i].bytes);
    free(pixels); free(one_allocation); free(four_allocation);
}
int main(int argc,char **argv) {
    fr_native_quiet();
    if (argc==1) {
        roundtrip(192,128,20,0);
        roundtrip(1366,768,20,0);
    } else if (argc==2 && (!strcmp(argv[1],"desktop") || !strcmp(argv[1],"noise"))) {
        roundtrip(1920,1080,MAX_UNITS,!strcmp(argv[1],"noise"));
    } else {
        fprintf(stderr,"usage: decoder-threads [desktop|noise]\n"); return 2;
    }
    return 0;
}
