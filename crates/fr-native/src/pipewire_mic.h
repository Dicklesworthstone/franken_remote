#ifndef FR_PIPEWIRE_MIC_H
#define FR_PIPEWIRE_MIC_H
#include <stddef.h>
#include <stdint.h>

/* A thread-confined PipeWire source. The clock callback is content-free and
 * nonblocking; its context lives until fr_pipewire_mic_free has returned. */
typedef struct FrPipewireMic FrPipewireMic;
typedef int (*FrMicClock)(const void *, uint64_t *);
typedef int (*FrMicPermission)(void *);
/* Fixed, content-free first-failure evidence. No strings, PCM, allocation or
 * callback logging. Codes are local diagnostic metadata, never wire values. */
typedef struct {
    uint32_t code, frames;
    uint64_t observed, expected;
    uint64_t cycle, stamp_nsec;
    /* Previous cycle/stamp name the last completed callback; previous_samples
     * can be a newer demand read's interpolated consumption estimate. */
    uint64_t previous_cycle, previous_samples, previous_stamp_nsec;
} FrMicFailure;

enum FrMicFailureCode {
    FR_MIC_NO_FAILURE = 0,
    FR_MIC_GRAPH_STATE = 1, FR_MIC_GRAPH_POSITION = 2,
    FR_MIC_GRAPH_SNAPSHOT = 3, FR_MIC_GRAPH_QUANTUM = 4,
    FR_MIC_GRAPH_ID_RACE = 5, FR_MIC_GRAPH_FUTURE_STAMP = 6,
    FR_MIC_GRAPH_ELAPSED = 7, FR_MIC_GRAPH_ARITHMETIC = 8,
    FR_MIC_GRAPH_CLOCK_CHANGED = 9, FR_MIC_GRAPH_RATE_CHANGED = 10,
    FR_MIC_GRAPH_QUANTUM_CHANGED = 11, FR_MIC_GRAPH_STAMP_REGRESSION = 12,
    FR_MIC_GRAPH_ESTIMATE_REGRESSION = 13, FR_MIC_GRAPH_DELAY = 14,
    FR_MIC_GRAPH_QUEUE = 15, FR_MIC_GRAPH_BUFFERED = 16,
    FR_MIC_PROCESS_CYCLE = 20, FR_MIC_PROCESS_NO_BUFFER = 21,
    FR_MIC_PROCESS_BUFFER = 22, FR_MIC_PROCESS_ARITHMETIC = 23,
    FR_MIC_PROCESS_BLOCK = 24, FR_MIC_PROCESS_UNDERRUN = 25,
    FR_MIC_PROCESS_FRESH_CYCLE = 26, FR_MIC_PROCESS_PUBLICATION = 27,
    FR_MIC_PROCESS_TAIL = 28, FR_MIC_PROCESS_QUEUE = 29,
    FR_MIC_PROCESS_NEXT_DEMAND = 30, FR_MIC_EXPIRED_BLOCK = 31,
    FR_MIC_CORE_ERROR = 40, FR_MIC_STREAM_STATE = 41,
    FR_MIC_POSITION_CHANGED = 42, FR_MIC_FORMAT = 43,
    FR_MIC_BUFFERS = 44, FR_MIC_LOOP = 45,
    FR_MIC_PROCESS_PERMISSION = 46, FR_MIC_RAW_BUFFER = 47,
    FR_MIC_RAW_QUEUE = 48, FR_MIC_DEMAND_INVALID = 50,
    FR_MIC_DEMAND_WALL = 51, FR_MIC_DEMAND_SAMPLES = 52,
    FR_MIC_SUBMIT_INVALID = 60, FR_MIC_SUBMIT_SERIAL = 61,
    FR_MIC_SUBMIT_INTERVAL = 62, FR_MIC_SUBMIT_ARITHMETIC = 63,
    FR_MIC_SUBMIT_PASSED = 64, FR_MIC_SUBMIT_CONTINUITY = 65,
    FR_MIC_POLL_INVALID = 70, FR_MIC_POLL_STATE = 71
};

typedef struct {
    uint64_t submitted_frames;
    uint64_t silent_frames;
    uint64_t expired_frames;
    uint32_t queued_frames;
    uint32_t node_id;
    FrMicFailure failure;
} FrMicStatistics;

/* A real graph request, not a submission-counter clock. The interval names
 * one preparation cycle plus the asynchronous native handoff; until is fixed
 * when that request is observed. */
typedef struct {
    uint64_t serial;
    uint64_t graph_samples;
    uint64_t output_start;
    uint64_t output_end;
    uint64_t until;
    uint32_t available_blocks;
    uint32_t frames;
} FrMicDemand;

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
/* Select demand mode before admitting any raw PCM. */
int fr_pipewire_mic_enable_demand(FrPipewireMic *mic);
/* 1 real streaming demand, 0 no consumer yet, -1 terminal graph failure. */
int fr_pipewire_mic_demand(FrPipewireMic *mic, FrMicDemand *out);
/* Admission preserves the exact original request/interval/deadline. A stale
 * request cannot be retried under the next token. Same return codes as submit. */
int fr_pipewire_mic_submit_demand(FrPipewireMic *mic, const int16_t *pcm,
                                 uint32_t frames, uint64_t serial,
                                 uint64_t start, uint64_t until);
void fr_pipewire_mic_statistics(const FrPipewireMic *mic, FrMicStatistics *out);
void fr_pipewire_mic_free(FrPipewireMic *mic);
#endif
