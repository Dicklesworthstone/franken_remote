/* Project-owned C ABI boundary wrapping curated FFmpeg libavcodec/libavutil.
 * Strictly thread-confined. Foreign types never cross into Rust media/wire APIs.
 * Memory safety, pointer provenance, and shutdown order documented per AGENTS.md §3.2.
 */
#include "ffmpeg_bridge.h"

#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <errno.h>

#ifndef FR_FFI_STUB_ONLY
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>
#include <libavutil/hwcontext.h>
#include <libavutil/imgutils.h>
#include <libswscale/swscale.h>
#endif

/* Result status mapper from FFmpeg error codes */
static int map_av_error(int r) {
    if (r >= 0) return FR_FFI_OK;
#ifndef FR_FFI_STUB_ONLY
    if (r == AVERROR(EAGAIN)) return FR_FFI_AGAIN;
    if (r == AVERROR_EOF) return FR_FFI_EOF;
    if (r == AVERROR(ENOMEM)) return FR_FFI_MEMORY;
    if (r == AVERROR(EINVAL)) return FR_FFI_INVALID;
#endif
    return FR_FFI_CODEC;
}

static int validate_geometry(int w, int h) {
    return w >= 16 && h >= 16 && w <= 8192 && h <= 8192 &&
           (int64_t)w * h <= 16777216 && !(w & 1) && !(h & 1);
}

struct FrFfiEncoder {
    int is_mock;
    int device_lost;
    int draining;
    int width;
    int height;
    int fps;
    int bitrate;
    int max_gop;
    int in_flight;
    uint64_t mock_frame_counter;
    int mock_force_idr;
#ifndef FR_FFI_STUB_ONLY
    AVCodecContext *ctx;
    AVFrame *frame;
    AVPacket *packet;
    struct SwsContext *sws;
    AVBufferRef *device;
    AVBufferRef *hw_frames;
#endif
};

struct FrFfiDecoder {
    int is_mock;
    int device_lost;
    int draining;
    int width;
    int height;
    uint64_t mock_frame_counter;
#ifndef FR_FFI_STUB_ONLY
    AVCodecContext *ctx;
    AVFrame *frame;
    AVPacket *packet;
    struct SwsContext *sws;
    AVBufferRef *device;
    AVBufferRef *hw_frames;
#endif
};

void fr_ffi_set_quiet(void) {
#ifndef FR_FFI_STUB_ONLY
    av_log_set_level(AV_LOG_QUIET);
#endif
}

uint32_t fr_ffi_avcodec_version(void) {
#ifndef FR_FFI_STUB_ONLY
    return avcodec_version();
#else
    return 0;
#endif
}

void fr_ffi_simulate_device_loss(FrFfiEncoder *enc) {
    if (enc) {
        enc->device_lost = 1;
    }
}

int fr_ffi_encoder_new(
    int backend,
    int width,
    int height,
    int fps,
    int bitrate,
    int max_gop,
    FrFfiEncoder **out
) {
    if (!out) return FR_FFI_INVALID;
    *out = NULL;
    if (!validate_geometry(width, height) || fps < 1 || fps > 240 ||
        bitrate < 10000 || bitrate > 200000000 || max_gop < 1 || max_gop > 480) {
        return FR_FFI_INVALID;
    }

    FrFfiEncoder *e = (FrFfiEncoder *)calloc(1, sizeof(FrFfiEncoder));
    if (!e) return FR_FFI_MEMORY;

    e->width = width;
    e->height = height;
    e->fps = fps;
    e->bitrate = bitrate;
    e->max_gop = max_gop;

    if (backend == -1) {
        /* Mock / simulated encoder for unit tests and fault injection */
        e->is_mock = 1;
        *out = e;
        return FR_FFI_OK;
    }

#ifndef FR_FFI_STUB_ONLY
    const char *name = NULL;
    switch (backend) {
        case 0: name = "hevc_nvenc"; break;
        case 1: name = "hevc_vaapi"; break;
        case 2: name = "hevc_amf"; break;
        case 3: name = "hevc_qsv"; break;
        case 4: name = "libx265"; break;
        default: free(e); return FR_FFI_INVALID;
    }

    const AVCodec *codec = avcodec_find_encoder_by_name(name);
    if (!codec || codec->id != AV_CODEC_ID_HEVC) {
        free(e);
        return FR_FFI_UNAVAILABLE;
    }

    e->ctx = avcodec_alloc_context3(codec);
    e->frame = av_frame_alloc();
    e->packet = av_packet_alloc();
    if (!e->ctx || !e->frame || !e->packet) {
        fr_ffi_encoder_free(e);
        return FR_FFI_MEMORY;
    }

    AVCodecContext *c = e->ctx;
    c->width = width;
    c->height = height;
    c->time_base = (AVRational){1, (int)fps};
    c->framerate = (AVRational){(int)fps, 1};
    c->bit_rate = bitrate;
    c->gop_size = max_gop;
    c->max_b_frames = 0; /* Zero reordering for ultra-low-latency real-time */
    c->flags |= AV_CODEC_FLAG_LOW_DELAY;
    c->pix_fmt = (backend == 1) ? AV_PIX_FMT_VAAPI : AV_PIX_FMT_YUV420P;

    /* Vendor-specific zerolatency / real-time tuning */
    if (backend == 0) { /* NVENC */
        av_opt_set(c->priv_data, "preset", "ll", 0);
        av_opt_set(c->priv_data, "tune", "zerolatency", 0);
        av_opt_set(c->priv_data, "delay", "0", 0);
    } else if (backend == 4) { /* libx265 */
        av_opt_set(c->priv_data, "preset", "ultrafast", 0);
        av_opt_set(c->priv_data, "tune", "zerolatency", 0);
    }

    /* Allocate frame buffer */
    e->frame->format = AV_PIX_FMT_YUV420P;
    e->frame->width = width;
    e->frame->height = height;
    if (av_frame_get_buffer(e->frame, 32) < 0) {
        fr_ffi_encoder_free(e);
        return FR_FFI_MEMORY;
    }

    /* SwsContext for converting input BGRA to YUV420P */
    e->sws = sws_getContext(
        width, height, AV_PIX_FMT_BGRA,
        width, height, AV_PIX_FMT_YUV420P,
        SWS_FAST_BILINEAR | SWS_ACCURATE_RND,
        NULL, NULL, NULL
    );
    if (!e->sws) {
        fr_ffi_encoder_free(e);
        return FR_FFI_MEMORY;
    }

    if (avcodec_open2(c, codec, NULL) < 0) {
        fr_ffi_encoder_free(e);
        return FR_FFI_UNAVAILABLE;
    }

    *out = e;
    return FR_FFI_OK;
#else
    free(e);
    return FR_FFI_UNAVAILABLE;
#endif
}

int fr_ffi_encoder_send_frame(
    FrFfiEncoder *enc,
    const uint8_t *bgra_data,
    size_t bgra_len,
    uint64_t pts,
    int force_idr
) {
    if (!enc) return FR_FFI_INVALID;
    if (enc->device_lost) return FR_FFI_DEVICE_LOST;
    if (bgra_len < (size_t)enc->width * (size_t)enc->height * 4) return FR_FFI_INVALID;

    if (enc->is_mock) {
        if (enc->in_flight >= 4) return FR_FFI_AGAIN; /* Backpressure after 4 frames */
        enc->in_flight++;
        enc->mock_frame_counter = pts;
        enc->mock_force_idr = force_idr;
        return FR_FFI_OK;
    }

#ifndef FR_FFI_STUB_ONLY
    if (!enc->ctx || !enc->frame) return FR_FFI_INVALID;

    const uint8_t *src_slices[4] = { bgra_data, NULL, NULL, NULL };
    int src_strides[4] = { enc->width * 4, 0, 0, 0 };

    if (av_frame_make_writable(enc->frame) < 0) {
        return FR_FFI_MEMORY;
    }

    sws_scale(
        enc->sws,
        src_slices, src_strides, 0, enc->height,
        enc->frame->data, enc->frame->linesize
    );

    enc->frame->pts = (int64_t)pts;
    if (force_idr) {
        enc->frame->pict_type = AV_PICTURE_TYPE_I;
#if LIBAVUTIL_VERSION_MAJOR < 58
        enc->frame->key_frame = 1;
#else
        enc->frame->flags |= AV_FRAME_FLAG_KEY;
#endif
    } else {
        enc->frame->pict_type = AV_PICTURE_TYPE_NONE;
#if LIBAVUTIL_VERSION_MAJOR < 58
        enc->frame->key_frame = 0;
#else
        enc->frame->flags &= ~AV_FRAME_FLAG_KEY;
#endif
    }

    int r = avcodec_send_frame(enc->ctx, enc->frame);
    return map_av_error(r);
#else
    return FR_FFI_UNAVAILABLE;
#endif
}

int fr_ffi_encoder_receive_packet(
    FrFfiEncoder *enc,
    uint8_t *out_buf,
    size_t out_capacity,
    size_t *out_len,
    uint64_t *out_pts,
    int *out_is_idr
) {
    if (!enc || !out_buf || !out_len || !out_pts || !out_is_idr) return FR_FFI_INVALID;
    if (enc->device_lost) return FR_FFI_DEVICE_LOST;

    if (enc->is_mock) {
        if (enc->in_flight <= 0) return FR_FFI_AGAIN; /* NeedMoreInput */
        enc->in_flight--;
        *out_pts = enc->mock_frame_counter;
        *out_is_idr = (enc->mock_force_idr || enc->mock_frame_counter == 1) ? 1 : 0;
        /* Synthetic NAL unit: 4-byte length prefix + 1-byte NAL header + 8-byte frame ID */
        const size_t payload_len = 13;
        if (out_capacity < payload_len) return FR_FFI_MEMORY;
        uint32_t nal_len = 9;
        out_buf[0] = (uint8_t)(nal_len >> 24);
        out_buf[1] = (uint8_t)(nal_len >> 16);
        out_buf[2] = (uint8_t)(nal_len >> 8);
        out_buf[3] = (uint8_t)(nal_len);
        /* NAL unit header: IDR (type 19) or TRAIL_R (type 1) */
        out_buf[4] = (*out_is_idr) ? (19 << 1) : (1 << 1);
        for (int i = 0; i < 8; i++) {
            out_buf[5 + i] = (uint8_t)(enc->mock_frame_counter >> (56 - i * 8));
        }
        *out_len = payload_len;
        return FR_FFI_OK;
    }

#ifndef FR_FFI_STUB_ONLY
    if (!enc->ctx || !enc->packet) return FR_FFI_INVALID;

    int r = avcodec_receive_packet(enc->ctx, enc->packet);
    if (r < 0) return map_av_error(r);

    if ((size_t)enc->packet->size > out_capacity) {
        av_packet_unref(enc->packet);
        return FR_FFI_MEMORY;
    }

    memcpy(out_buf, enc->packet->data, enc->packet->size);
    *out_len = (size_t)enc->packet->size;
    *out_pts = (uint64_t)enc->packet->pts;
    *out_is_idr = (enc->packet->flags & AV_PKT_FLAG_KEY) ? 1 : 0;

    av_packet_unref(enc->packet);
    return FR_FFI_OK;
#else
    return FR_FFI_UNAVAILABLE;
#endif
}

int fr_ffi_encoder_drain(FrFfiEncoder *enc) {
    if (!enc) return FR_FFI_INVALID;
    if (enc->device_lost) return FR_FFI_DEVICE_LOST;
    enc->draining = 1;

    if (enc->is_mock) {
        return FR_FFI_OK;
    }

#ifndef FR_FFI_STUB_ONLY
    if (!enc->ctx) return FR_FFI_INVALID;
    /* Submitting NULL frame triggers drain / end-of-stream in FFmpeg */
    int r = avcodec_send_frame(enc->ctx, NULL);
    return map_av_error(r);
#else
    return FR_FFI_UNAVAILABLE;
#endif
}

void fr_ffi_encoder_free(FrFfiEncoder *enc) {
    if (!enc) return;
#ifndef FR_FFI_STUB_ONLY
    if (enc->packet) av_packet_free(&enc->packet);
    if (enc->frame) av_frame_free(&enc->frame);
    if (enc->sws) sws_freeContext(enc->sws);
    if (enc->ctx) avcodec_free_context(&enc->ctx);
    if (enc->hw_frames) av_buffer_unref(&enc->hw_frames);
    if (enc->device) av_buffer_unref(&enc->device);
#endif
    free(enc);
}

int fr_ffi_decoder_new(
    int width,
    int height,
    const uint8_t *extradata,
    size_t extradata_len,
    FrFfiDecoder **out
) {
    if (!out) return FR_FFI_INVALID;
    *out = NULL;
    if (!validate_geometry(width, height)) {
        return FR_FFI_INVALID;
    }

    FrFfiDecoder *d = (FrFfiDecoder *)calloc(1, sizeof(FrFfiDecoder));
    if (!d) return FR_FFI_MEMORY;

    d->width = width;
    d->height = height;

    if (extradata_len == 0) {
        /* Mock / test mode */
        d->is_mock = 1;
        *out = d;
        return FR_FFI_OK;
    }

#ifndef FR_FFI_STUB_ONLY
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_HEVC);
    if (!codec) {
        free(d);
        return FR_FFI_UNAVAILABLE;
    }

    d->ctx = avcodec_alloc_context3(codec);
    d->frame = av_frame_alloc();
    d->packet = av_packet_alloc();
    if (!d->ctx || !d->frame || !d->packet) {
        fr_ffi_decoder_free(d);
        return FR_FFI_MEMORY;
    }

    d->ctx->width = width;
    d->ctx->height = height;
    d->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;

    /* Extradata allocation with mandatory AV_INPUT_BUFFER_PADDING_SIZE */
    if (extradata && extradata_len > 0) {
        d->ctx->extradata = (uint8_t *)av_mallocz(extradata_len + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!d->ctx->extradata) {
            fr_ffi_decoder_free(d);
            return FR_FFI_MEMORY;
        }
        memcpy(d->ctx->extradata, extradata, extradata_len);
        d->ctx->extradata_size = (int)extradata_len;
    }

    if (avcodec_open2(d->ctx, codec, NULL) < 0) {
        fr_ffi_decoder_free(d);
        return FR_FFI_CODEC;
    }

    /* Output SwsContext: from decoded YUV420P to packed BGRA */
    d->sws = sws_getContext(
        width, height, AV_PIX_FMT_YUV420P,
        width, height, AV_PIX_FMT_BGRA,
        SWS_FAST_BILINEAR,
        NULL, NULL, NULL
    );
    if (!d->sws) {
        fr_ffi_decoder_free(d);
        return FR_FFI_MEMORY;
    }

    *out = d;
    return FR_FFI_OK;
#else
    free(d);
    return FR_FFI_UNAVAILABLE;
#endif
}

int fr_ffi_decoder_send_packet(
    FrFfiDecoder *dec,
    const uint8_t *data,
    size_t len,
    uint64_t pts
) {
    if (!dec) return FR_FFI_INVALID;
    if (dec->device_lost) return FR_FFI_DEVICE_LOST;

    if (dec->is_mock) {
        dec->mock_frame_counter = pts;
        return FR_FFI_OK;
    }

#ifndef FR_FFI_STUB_ONLY
    if (!dec->ctx) return FR_FFI_INVALID;

    /* Allocate packet with mandatory zeroed AV_INPUT_BUFFER_PADDING_SIZE */
    AVPacket *pkt = av_packet_alloc();
    if (!pkt) return FR_FFI_MEMORY;

    if (av_new_packet(pkt, (int)len) < 0) {
        av_packet_free(&pkt);
        return FR_FFI_MEMORY;
    }

    memcpy(pkt->data, data, len);
    memset(pkt->data + len, 0, AV_INPUT_BUFFER_PADDING_SIZE);
    pkt->pts = (int64_t)pts;

    int r = avcodec_send_packet(dec->ctx, pkt);
    av_packet_free(&pkt);
    return map_av_error(r);
#else
    return FR_FFI_UNAVAILABLE;
#endif
}

int fr_ffi_decoder_receive_frame(
    FrFfiDecoder *dec,
    uint8_t *out_bgra,
    size_t out_capacity,
    uint64_t *out_pts
) {
    if (!dec || !out_bgra || !out_pts) return FR_FFI_INVALID;
    if (dec->device_lost) return FR_FFI_DEVICE_LOST;
    const size_t expected_size = (size_t)dec->width * (size_t)dec->height * 4;
    if (out_capacity < expected_size) return FR_FFI_MEMORY;

    if (dec->is_mock) {
        *out_pts = dec->mock_frame_counter;
        memset(out_bgra, 0, expected_size);
        return FR_FFI_OK;
    }

#ifndef FR_FFI_STUB_ONLY
    if (!dec->ctx || !dec->frame) return FR_FFI_INVALID;

    int r = avcodec_receive_frame(dec->ctx, dec->frame);
    if (r < 0) return map_av_error(r);

    uint8_t *dst_slices[4] = { out_bgra, NULL, NULL, NULL };
    int dst_strides[4] = { dec->width * 4, 0, 0, 0 };

    sws_scale(
        dec->sws,
        (const uint8_t *const *)dec->frame->data, dec->frame->linesize,
        0, dec->height,
        dst_slices, dst_strides
    );

    *out_pts = (uint64_t)dec->frame->pts;
    av_frame_unref(dec->frame);
    return FR_FFI_OK;
#else
    return FR_FFI_UNAVAILABLE;
#endif
}

int fr_ffi_decoder_drain(FrFfiDecoder *dec) {
    if (!dec) return FR_FFI_INVALID;
    if (dec->device_lost) return FR_FFI_DEVICE_LOST;
    dec->draining = 1;

    if (dec->is_mock) {
        return FR_FFI_OK;
    }

#ifndef FR_FFI_STUB_ONLY
    if (!dec->ctx) return FR_FFI_INVALID;
    int r = avcodec_send_packet(dec->ctx, NULL);
    return map_av_error(r);
#else
    return FR_FFI_UNAVAILABLE;
#endif
}

void fr_ffi_decoder_free(FrFfiDecoder *dec) {
    if (!dec) return;
#ifndef FR_FFI_STUB_ONLY
    if (dec->packet) av_packet_free(&dec->packet);
    if (dec->frame) av_frame_free(&dec->frame);
    if (dec->sws) sws_freeContext(dec->sws);
    if (dec->ctx) avcodec_free_context(&dec->ctx);
    if (dec->hw_frames) av_buffer_unref(&dec->hw_frames);
    if (dec->device) av_buffer_unref(&dec->device);
#endif
    free(dec);
}
