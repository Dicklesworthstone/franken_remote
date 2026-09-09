/* The only C ABI boundary for the Linux CPU-staging media path. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <limits.h>
#include <errno.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>
#include <libavutil/hwcontext.h>
#include <libswscale/swscale.h>

/* Positive results are progress, negative results are bounded categories. */
enum { FR_OK=0, FR_AGAIN=1, FR_EOF=2, FR_INVALID=-1, FR_UNAVAILABLE=-2,
       FR_MEMORY=-3, FR_CODEC=-4, FR_GEOMETRY=-5, FR_DISPLAY=-6 };
static int result(int r) {
    if (r >= 0) return FR_OK;
    if (r == AVERROR(EAGAIN)) return FR_AGAIN;
    if (r == AVERROR_EOF) return FR_EOF;
    if (r == AVERROR(ENOMEM)) return FR_MEMORY;
    return FR_CODEC;
}
static int geometry(int w, int h) {
    return w >= 16 && h >= 16 && w <= 8192 && h <= 8192 &&
           (int64_t)w*h <= 16777216 && !(w&1) && !(h&1);
}
static int bgra_buffer(int w,int h, size_t len) {
    return geometry(w,h) && len == (size_t)w*(size_t)h*4;
}
void fr_native_quiet(void) { av_log_set_level(AV_LOG_QUIET); }
uint32_t fr_native_avcodec_version(void) { return avcodec_version(); }
uint32_t fr_native_compiled_avcodec_version(void) { return LIBAVCODEC_VERSION_INT; }

typedef struct {
    AVCodecContext *ctx;
    AVFrame *input;
    AVPacket *packet;
    struct SwsContext *scale;
    AVBufferRef *device, *frames;
    int have_packet, draining, hardware;
} FrEncoder;
void fr_encoder_free(FrEncoder *e) {
    if (!e) return;
    av_packet_free(&e->packet); av_frame_free(&e->input);
    avcodec_free_context(&e->ctx); sws_freeContext(e->scale);
    av_buffer_unref(&e->frames); av_buffer_unref(&e->device); free(e);
}
int fr_encoder_new(int backend,int w,int h,int fps,int bitrate,int gop,FrEncoder **out) {
    if (!out) return FR_INVALID; *out=NULL;
    if (!geometry(w,h) || fps<1 || fps>240 || bitrate<10000 || bitrate>200000000 || gop<1 || gop>480) return FR_INVALID;
    const char *name = backend==0?"hevc_nvenc":backend==1?"hevc_vaapi":backend==2?"libx265":NULL;
    if (!name) return FR_INVALID;
    const AVCodec *codec=avcodec_find_encoder_by_name(name);
    if (!codec || codec->id!=AV_CODEC_ID_HEVC) return FR_UNAVAILABLE;
    FrEncoder *e=calloc(1,sizeof(*e)); if (!e) return FR_MEMORY;
    e->ctx=avcodec_alloc_context3(codec); e->input=av_frame_alloc(); e->packet=av_packet_alloc();
    if (!e->ctx || !e->input || !e->packet) { fr_encoder_free(e); return FR_MEMORY; }
    AVCodecContext *c=e->ctx; c->width=w; c->height=h;
    c->time_base=(AVRational){1,fps}; c->framerate=(AVRational){fps,1};
    c->bit_rate=bitrate; c->gop_size=gop; c->max_b_frames=0; c->refs=1;
    c->thread_count=1; c->flags|=AV_CODEC_FLAG_LOW_DELAY;
    c->color_primaries=AVCOL_PRI_BT709; c->color_trc=AVCOL_TRC_BT709;
    c->colorspace=AVCOL_SPC_BT709; c->color_range=AVCOL_RANGE_MPEG;
    enum AVPixelFormat sw=backend==1?AV_PIX_FMT_NV12:AV_PIX_FMT_YUV420P;
    c->pix_fmt=sw; e->hardware=backend!=2;
    if (backend==1) {
        if (av_hwdevice_ctx_create(&e->device,AV_HWDEVICE_TYPE_VAAPI,NULL,NULL,0)<0) { fr_encoder_free(e); return FR_UNAVAILABLE; }
        e->frames=av_hwframe_ctx_alloc(e->device);
        if (!e->frames) { fr_encoder_free(e); return FR_MEMORY; }
        AVHWFramesContext *f=(AVHWFramesContext *)e->frames->data;
        f->format=AV_PIX_FMT_VAAPI; f->sw_format=sw; f->width=w; f->height=h; f->initial_pool_size=4;
        if (av_hwframe_ctx_init(e->frames)<0) { fr_encoder_free(e); return FR_UNAVAILABLE; }
        c->hw_frames_ctx=av_buffer_ref(e->frames);
        if (!c->hw_frames_ctx) { fr_encoder_free(e); return FR_MEMORY; }
        c->pix_fmt=AV_PIX_FMT_VAAPI;
    }
    AVDictionary *opts=NULL;
    av_dict_set(&opts,"profile","main",0);
    if (backend==0) {
        av_dict_set(&opts,"preset","p1",0); av_dict_set(&opts,"tune","ull",0);
        av_dict_set(&opts,"zerolatency","1",0); av_dict_set(&opts,"rc-lookahead","0",0);
        av_dict_set(&opts,"forced-idr","1",0); av_dict_set(&opts,"delay","0",0);
    } else if (backend==2) {
        char params[256];
        snprintf(params,sizeof(params),"bframes=0:rc-lookahead=0:ref=1:keyint=%d:min-keyint=%d:scenecut=0:open-gop=0:repeat-headers=1:aud=1:pools=none:frame-threads=1:log-level=error",gop,gop);
        av_dict_set(&opts,"preset","ultrafast",0); av_dict_set(&opts,"tune","zerolatency",0);
        av_dict_set(&opts,"x265-params",params,0); av_dict_set(&opts,"forced-idr","1",0);
    }
    int r=avcodec_open2(c,codec,&opts), unused=av_dict_count(opts); av_dict_free(&opts);
    if (r<0 || unused) { fr_encoder_free(e); return FR_UNAVAILABLE; }
    e->input->format=sw; e->input->width=w; e->input->height=h;
    e->input->color_primaries=c->color_primaries; e->input->color_trc=c->color_trc;
    e->input->colorspace=c->colorspace; e->input->color_range=c->color_range;
    if (av_frame_get_buffer(e->input,32)<0) { fr_encoder_free(e); return FR_MEMORY; }
    e->scale=sws_getContext(w,h,AV_PIX_FMT_BGRA,w,h,sw,SWS_FAST_BILINEAR,NULL,NULL,NULL);
    if (!e->scale || sws_setColorspaceDetails(e->scale,sws_getCoefficients(SWS_CS_ITU709),1,sws_getCoefficients(SWS_CS_ITU709),0,0,1<<16,1<<16)<0) { fr_encoder_free(e); return FR_CODEC; }
    *out=e; return FR_OK;
}
int fr_encoder_send(FrEncoder *e,const uint8_t *bgra,size_t len,int64_t pts,int idr) {
    if (!e || !bgra || pts<0 || e->draining || !bgra_buffer(e->ctx->width,e->ctx->height,len)) return FR_INVALID;
    if (av_frame_make_writable(e->input)<0) return FR_MEMORY;
    const uint8_t *src[4]={bgra,NULL,NULL,NULL}; int strides[4]={e->ctx->width*4,0,0,0};
    if (sws_scale(e->scale,src,strides,0,e->ctx->height,e->input->data,e->input->linesize)!=e->ctx->height) return FR_CODEC;
    e->input->pts=pts; e->input->pict_type=idr?AV_PICTURE_TYPE_I:AV_PICTURE_TYPE_NONE;
    if (e->frames) {
        AVFrame *gpu=av_frame_alloc(); if (!gpu) return FR_MEMORY;
        int r=av_hwframe_get_buffer(e->frames,gpu,0);
        if (r>=0) r=av_hwframe_transfer_data(gpu,e->input,0);
        if (r>=0) r=av_frame_copy_props(gpu,e->input);
        if (r>=0) r=avcodec_send_frame(e->ctx,gpu);
        av_frame_free(&gpu); return result(r);
    }
    return result(avcodec_send_frame(e->ctx,e->input));
}
int fr_encoder_drain(FrEncoder *e) {
    if (!e) return FR_INVALID;
    if (e->draining) return FR_OK;
    int r=avcodec_send_frame(e->ctx,NULL); if (r>=0) e->draining=1; return result(r);
}
int fr_encoder_peek(FrEncoder *e,size_t *len,int64_t *pts) {
    if (!e || !len || !pts) return FR_INVALID;
    if (!e->have_packet) {
        int r=avcodec_receive_packet(e->ctx,e->packet); if (r<0) return result(r);
        e->have_packet=1;
    }
    if (e->packet->size<=0 || e->packet->pts<0) return FR_CODEC;
    *len=(size_t)e->packet->size; *pts=e->packet->pts; return FR_OK;
}
int fr_encoder_take(FrEncoder *e,uint8_t *dst,size_t len) {
    if (!e || !dst || !e->have_packet || len!=(size_t)e->packet->size) return FR_INVALID;
    memcpy(dst,e->packet->data,len); av_packet_unref(e->packet); e->have_packet=0; return FR_OK;
}

typedef struct { AVCodecContext *ctx; AVFrame *frame; struct SwsContext *scale; int w,h,have_frame; } FrDecoder;
void fr_decoder_free(FrDecoder *d) {
    if (!d) return; avcodec_free_context(&d->ctx); av_frame_free(&d->frame); sws_freeContext(d->scale); free(d);
}
int fr_decoder_new(int w,int h,int coded_w,int coded_h,const uint8_t *configuration,size_t configuration_len,FrDecoder **out) {
    if (!out) return FR_INVALID; *out=NULL;
    if (!geometry(w,h) || !geometry(coded_w,coded_h) || w>coded_w || h>coded_h) return FR_INVALID;
    if (!configuration || configuration_len<23 || configuration_len>12326) return FR_INVALID;
    const AVCodec *codec=avcodec_find_decoder(AV_CODEC_ID_HEVC); if (!codec) return FR_UNAVAILABLE;
    FrDecoder *d=calloc(1,sizeof(*d)); if (!d) return FR_MEMORY;
    d->ctx=avcodec_alloc_context3(codec); d->frame=av_frame_alloc(); d->w=w; d->h=h;
    if (!d->ctx || !d->frame) { fr_decoder_free(d); return FR_MEMORY; }
    d->ctx->width=w; d->ctx->height=h; d->ctx->pix_fmt=AV_PIX_FMT_YUV420P;
    d->ctx->thread_count=1; d->ctx->thread_type=FF_THREAD_SLICE;
    d->ctx->err_recognition=AV_EF_EXPLODE|AV_EF_CAREFUL; d->ctx->flags|=AV_CODEC_FLAG_LOW_DELAY;
    /* Rust admits exact VPS/SPS/PPS before this boundary. FFmpeg owns the copy
       and requires zeroed SIMD padding even for configuration-only input. */
    d->ctx->extradata=av_mallocz(configuration_len+AV_INPUT_BUFFER_PADDING_SIZE);
    if (!d->ctx->extradata) { fr_decoder_free(d); return FR_MEMORY; }
    memcpy(d->ctx->extradata,configuration,configuration_len);
    d->ctx->extradata_size=(int)configuration_len;
    if (avcodec_open2(d->ctx,codec,NULL)<0) { fr_decoder_free(d); return FR_UNAVAILABLE; }
    /* max_pixels also checks SIMD-aligned rows in FFmpeg 6.1 ff_get_buffer.
       SPS coded pixels are NOT that allocation envelope: 1366 visible pixels
       need 1376 coded pixels but can need 1408 allocation pixels per row.
       Ask the selected ABI for its planar alignment instead of disabling the
       native cap or hardcoding a SIMD width. The Rust HEVC guard still checks
       exact coded/crop/profile/DPB limits before every avcodec_send_packet. */
    int allocation_w=coded_w, allocation_h=coded_h;
    avcodec_align_dimensions(d->ctx,&allocation_w,&allocation_h);
    if (allocation_w<coded_w || allocation_h<coded_h ||
        allocation_w-coded_w>256 || allocation_h-coded_h>256 ||
        allocation_w>8192 || allocation_h>8192 ||
        (int64_t)allocation_w*allocation_h>33554432) {
        fr_decoder_free(d); return FR_INVALID;
    }
    d->ctx->max_pixels=(int64_t)allocation_w*allocation_h;
    *out=d; return FR_OK;
}
int fr_decoder_send(FrDecoder *d,const uint8_t *bytes,size_t len,int64_t pts) {
    if (!d || !bytes || !len || len>16777216 || pts<0) return FR_INVALID;
    AVPacket *p=av_packet_alloc(); if (!p) return FR_MEMORY;
    int r=av_new_packet(p,(int)len);
    if (r>=0) { /* av_new_packet supplies zeroed AV_INPUT_BUFFER_PADDING_SIZE bytes. */
        memcpy(p->data,bytes,len); p->pts=pts; p->dts=pts; r=avcodec_send_packet(d->ctx,p);
    }
    av_packet_free(&p); return result(r);
}
int fr_decoder_receive(FrDecoder *d,uint8_t *out,size_t len,int64_t *pts) {
    if (!d || !out || !pts || !bgra_buffer(d->w,d->h,len)) return FR_INVALID;
    if (!d->have_frame) { int r=avcodec_receive_frame(d->ctx,d->frame); if (r<0) return result(r); d->have_frame=1; }
    AVFrame *f=d->frame;
    if (f->width!=d->w || f->height!=d->h || f->format!=AV_PIX_FMT_YUV420P || f->pts<0 || f->decode_error_flags) return FR_GEOMETRY;
    d->scale=sws_getCachedContext(d->scale,d->w,d->h,(enum AVPixelFormat)f->format,d->w,d->h,AV_PIX_FMT_BGRA,SWS_FAST_BILINEAR,NULL,NULL,NULL);
    if (!d->scale || sws_setColorspaceDetails(d->scale,sws_getCoefficients(SWS_CS_ITU709),0,sws_getCoefficients(SWS_CS_ITU709),1,0,1<<16,1<<16)<0) return FR_CODEC;
    uint8_t *dst[4]={out,NULL,NULL,NULL}; int stride[4]={d->w*4,0,0,0};
    if (sws_scale(d->scale,(const uint8_t *const *)f->data,f->linesize,0,d->h,dst,stride)!=d->h) return FR_CODEC;
    *pts=f->pts; av_frame_unref(f); d->have_frame=0; return FR_OK;
}

typedef struct { Display *display; Window window; GC gc; int screen,w,h,presenter; } FrX11;
void fr_x11_free(FrX11 *x) {
    if (!x) return;
    if (x->display) { if (x->gc) XFreeGC(x->display,x->gc); if (x->presenter && x->window) XDestroyWindow(x->display,x->window); XCloseDisplay(x->display); }
    free(x);
}
int fr_x11_new(const char *display,int presenter,int w,int h,FrX11 **out,int *width,int *height) {
    if (!out || !width || !height) return FR_INVALID; *out=NULL;
    FrX11 *x=calloc(1,sizeof(*x)); if (!x) return FR_MEMORY;
    x->display=XOpenDisplay(display); if (!x->display) { free(x); return FR_DISPLAY; }
    x->screen=DefaultScreen(x->display); x->presenter=presenter;
    Visual *v=DefaultVisual(x->display,x->screen);
    if (v->class!=TrueColor || v->red_mask!=0xff0000 || v->green_mask!=0xff00 || v->blue_mask!=0xff || DefaultDepth(x->display,x->screen)!=24) { fr_x11_free(x); return FR_UNAVAILABLE; }
    if (presenter) {
        if (!geometry(w,h)) { fr_x11_free(x); return FR_GEOMETRY; }
        x->w=w; x->h=h;
        x->window=XCreateSimpleWindow(x->display,RootWindow(x->display,x->screen),0,0,w,h,0,0,0);
        if (!x->window) { fr_x11_free(x); return FR_DISPLAY; }
        XStoreName(x->display,x->window,"FrankenRemote native media verification");
        XMapWindow(x->display,x->window); x->gc=XCreateGC(x->display,x->window,0,NULL); XSync(x->display,False);
    } else {
        x->window=RootWindow(x->display,x->screen); x->w=DisplayWidth(x->display,x->screen); x->h=DisplayHeight(x->display,x->screen);
        if (!geometry(x->w,x->h)) { fr_x11_free(x); return FR_GEOMETRY; }
    }
    *width=x->w; *height=x->h; *out=x; return FR_OK;
}
int fr_x11_capture(FrX11 *x,uint8_t *out,size_t len) {
    if (!x || !out || !bgra_buffer(x->w,x->h,len)) return FR_INVALID;
    XWindowAttributes a;
    if (!XGetWindowAttributes(x->display,x->window,&a)) return FR_DISPLAY;
    if (a.width!=x->w || a.height!=x->h) return FR_GEOMETRY;
    XImage *image=XGetImage(x->display,x->window,0,0,x->w,x->h,AllPlanes,ZPixmap); if (!image) return FR_DISPLAY;
    if (image->bits_per_pixel!=32 || image->byte_order!=LSBFirst || image->bytes_per_line<x->w*4) { XDestroyImage(image); return FR_UNAVAILABLE; }
    for (int y=0;y<x->h;y++) memcpy(out+(size_t)y*x->w*4,image->data+(size_t)y*image->bytes_per_line,(size_t)x->w*4);
    for (size_t i=3;i<len;i+=4) out[i]=255;
    XDestroyImage(image); return FR_OK;
}
int fr_x11_present(FrX11 *x,const uint8_t *bgra,size_t len) {
    if (!x || !x->presenter || !bgra || !bgra_buffer(x->w,x->h,len)) return FR_INVALID;
    XImage *im=XCreateImage(x->display,DefaultVisual(x->display,x->screen),24,ZPixmap,0,NULL,x->w,x->h,32,0);
    if (!im) return FR_MEMORY;
    if (im->bits_per_pixel!=32 || im->byte_order!=LSBFirst || im->bytes_per_line<x->w*4) { XDestroyImage(im); return FR_UNAVAILABLE; }
    im->data=calloc((size_t)im->bytes_per_line,(size_t)x->h); if (!im->data) { XDestroyImage(im); return FR_MEMORY; }
    for (int y=0;y<x->h;y++) memcpy(im->data+(size_t)y*im->bytes_per_line,bgra+(size_t)y*x->w*4,(size_t)x->w*4);
    XRaiseWindow(x->display,x->window); XPutImage(x->display,x->window,x->gc,im,0,0,0,0,x->w,x->h); XSync(x->display,False); XDestroyImage(im); return FR_OK;
}
