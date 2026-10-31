#ifndef FR_PIPEWIRE_MIC_H
#define FR_PIPEWIRE_MIC_H
#include <stddef.h>
#include <stdint.h>

/* A thread-confined PipeWire source. The clock callback is content-free and
 * nonblocking; its context lives until fr_pipewire_mic_free has returned. */
typedef struct FrPipewireMic FrPipewireMic;
typedef int (*FrMicClock)(const void *, uint64_t *);
typedef int (*FrMicPermission)(void *);
typedef struct {
    uint64_t submitted_frames;
    uint64_t silent_frames;
    uint64_t expired_frames;
    uint32_t queued_frames;
    uint32_t node_id;
} FrMicStatistics;

void fr_pipewire_mic_init(void);
/* fd is a connected Unix socket, consumed on every return path. */
int fr_pipewire_mic_new(int fd, const char *name, uint32_t channels,
                       FrMicClock clock, const void *context, FrPipewireMic **out);
/* Nonblocking main-loop turns. 0 connecting, 1 selectable, 2 stopped, -1 failed. */
/* Permission/context are borrowed ONLY for this synchronous main-loop turn. */
int fr_pipewire_mic_poll(FrPipewireMic *mic, FrMicPermission permission, void *context);
/* 0 accepted, 1 backpressure, 2 expired, 3 stopped, -1 invalid/unavailable.
 * Deadlines use the callback's original monotonic microsecond clock. */
int fr_pipewire_mic_submit(FrPipewireMic *mic, const int16_t *pcm,
                          uint32_t frames, uint64_t until);
void fr_pipewire_mic_statistics(const FrPipewireMic *mic, FrMicStatistics *out);
void fr_pipewire_mic_free(FrPipewireMic *mic);
#endif
