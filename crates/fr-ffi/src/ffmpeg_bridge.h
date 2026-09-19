#ifndef FR_FFI_BRIDGE_H
#define FR_FFI_BRIDGE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Result status codes matching FrankenRemote typed classifications */
enum {
    FR_FFI_OK = 0,
    FR_FFI_AGAIN = 1,        /* Backpressure (send) or NeedMoreInput (receive) */
    FR_FFI_EOF = 2,          /* End of stream */
    FR_FFI_INVALID = -1,     /* Invalid arguments or parameters */
    FR_FFI_UNAVAILABLE = -2, /* Requested hardware or codec unavailable */
    FR_FFI_MEMORY = -3,      /* Allocation failure */
    FR_FFI_CODEC = -4,       /* Codec internal error */
    FR_FFI_DEVICE_LOST = -5, /* GPU device lost or reset */
    FR_FFI_FATAL = -6        /* Fatal corruption */
};

/* Opaque pointer types */
typedef struct FrFfiEncoder FrFfiEncoder;
typedef struct FrFfiDecoder FrFfiDecoder;

/* Encoder lifecycle */
int fr_ffi_encoder_new(
    int backend, /* 0: nvenc, 1: vaapi, 2: amf, 3: qsv, 4: software (x265), -1: mock/simulated */
    int width,
    int height,
    int fps,
    int bitrate,
    int max_gop,
    FrFfiEncoder **out
);

int fr_ffi_encoder_send_frame(
    FrFfiEncoder *enc,
    const uint8_t *bgra_data,
    size_t bgra_len,
    uint64_t pts,
    int force_idr
);

int fr_ffi_encoder_receive_packet(
    FrFfiEncoder *enc,
    uint8_t *out_buf,
    size_t out_capacity,
    size_t *out_len,
    uint64_t *out_pts,
    int *out_is_idr
);

int fr_ffi_encoder_drain(FrFfiEncoder *enc);

void fr_ffi_encoder_free(FrFfiEncoder *enc);

/* Decoder lifecycle */
int fr_ffi_decoder_new(
    int width,
    int height,
    const uint8_t *extradata,
    size_t extradata_len,
    FrFfiDecoder **out
);

int fr_ffi_decoder_send_packet(
    FrFfiDecoder *dec,
    const uint8_t *data,
    size_t len,
    uint64_t pts
);

int fr_ffi_decoder_receive_frame(
    FrFfiDecoder *dec,
    uint8_t *out_bgra,
    size_t out_capacity,
    uint64_t *out_pts
);

int fr_ffi_decoder_drain(FrFfiDecoder *dec);

void fr_ffi_decoder_free(FrFfiDecoder *dec);

/* Query & diagnostics */
uint32_t fr_ffi_avcodec_version(void);
void fr_ffi_set_quiet(void);
void fr_ffi_simulate_device_loss(FrFfiEncoder *enc);

#ifdef __cplusplus
}
#endif

#endif /* FR_FFI_BRIDGE_H */
