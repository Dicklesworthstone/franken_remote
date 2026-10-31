/* A local PipeWire virtual microphone; never run on the authority thread.
 * Every buffer is checked at the native submission boundary. No callback
 * allocation, file access, logging, locks, resampling, or implicit device choice.
 * Process callbacks run on our explicitly polled main loop, not an RT thread.
 */
#define _GNU_SOURCE
#include "pipewire_mic.h"
#include <pipewire/pipewire.h>
#include <spa/param/audio/format-utils.h>
#include <spa/param/buffers.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define FR_MIC_BLOCKS 4u
#define FR_MIC_BLOCK_FRAMES 960u
#define FR_MIC_OUTPUT_FRAMES 4096u
#define FR_MIC_MAX_AGE_US 100000u

typedef struct {
    int16_t samples[FR_MIC_BLOCK_FRAMES * 2];
    uint32_t frames, read;
    uint64_t until;
} FrMicBlock;

struct FrPipewireMic {
    struct pw_loop *loop;
    struct pw_context *context;
    struct pw_core *core;
    struct pw_stream *stream;
    struct spa_hook stream_listener, core_listener;
    FrMicClock clock;
    const void *clock_context;
    FrMicPermission permission;
    void *permission_context;
    uint32_t channels, head, count;
    int failed, stopped;
    FrMicBlock blocks[FR_MIC_BLOCKS];
    FrMicStatistics statistics;
};

static void erase_block(FrMicBlock *block) {
    /* Volatile writes keep retired private PCM out of subsequent allocations. */
    volatile int16_t *samples = block->samples;
    for (uint32_t i = 0; i < FR_MIC_BLOCK_FRAMES * 2; i++) samples[i] = 0;
    block->frames = block->read = 0;
    block->until = 0;
}
static void retire_front(FrPipewireMic *mic) {
    erase_block(&mic->blocks[mic->head]);
    mic->head = (mic->head + 1) % FR_MIC_BLOCKS;
    mic->count--;
}
static void clear(FrPipewireMic *mic) {
    while (mic->count) retire_front(mic);
    mic->statistics.queued_frames = 0;
}
static int current(FrPipewireMic *mic, uint64_t *now) {
    /* A scoped original-authority check is installed during poll. Sample time
     * AFTER that check: even a delayed permission callback cannot refresh PCM. */
    if (mic->stopped ||
        (mic->permission && !mic->permission(mic->permission_context)) ||
        !mic->clock(mic->clock_context, now)) {
        mic->stopped = 1;
        clear(mic);
        return 0;
    }
    return 1;
}
static void expire(FrPipewireMic *mic, uint64_t now) {
    while (mic->count) {
        FrMicBlock *block = &mic->blocks[mic->head];
        if (now < block->until) break;
        uint32_t frames = block->frames - block->read;
        mic->statistics.expired_frames += frames;
        mic->statistics.queued_frames -= frames;
        retire_front(mic);
    }
}
static void on_core_error(void *data, uint32_t id, int seq, int res,
                          const char *message) {
    (void)id; (void)seq; (void)res; (void)message;
    /* Never retain or report untrusted native error strings. */
    ((FrPipewireMic *)data)->failed = 1;
}
static const struct pw_core_events core_events = {
    PW_VERSION_CORE_EVENTS,
    .error = on_core_error,
};
static void on_state(void *data, enum pw_stream_state old,
                     enum pw_stream_state state, const char *error) {
    (void)old; (void)error;
    if (state == PW_STREAM_STATE_ERROR) ((FrPipewireMic *)data)->failed = 1;
}
static void on_format(void *data, uint32_t id, const struct spa_pod *param) {
    FrPipewireMic *mic = data;
    if (id != SPA_PARAM_Format || !param) return;
    struct spa_audio_info_raw format = { 0 };
    if (spa_format_audio_raw_parse(param, &format) < 0 ||
        format.format != SPA_AUDIO_FORMAT_S16 || format.rate != 48000 ||
        format.channels != mic->channels) {
        mic->failed = 1;
        return;
    }
    uint8_t buffer[512];
    struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(buffer, sizeof buffer);
    const struct spa_pod *params[1];
    /* Two fixed-size native buffers: bounds include the hidden device handoff. */
    params[0] = spa_pod_builder_add_object(&builder,
        SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(2, 1, 2),
        SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
        SPA_PARAM_BUFFERS_size,
            SPA_POD_Int(FR_MIC_OUTPUT_FRAMES * mic->channels * sizeof(int16_t)),
        SPA_PARAM_BUFFERS_stride, SPA_POD_Int(mic->channels * sizeof(int16_t)));
    if (!params[0] || pw_stream_update_params(mic->stream, params, 1) < 0)
        mic->failed = 1;
}
static void on_process(void *data) {
    FrPipewireMic *mic = data;
    /* No RT_PROCESS: a process callback is legal only inside our scoped poll. */
    if (!mic->permission) mic->failed = 1;
    struct pw_buffer *buffer = pw_stream_dequeue_buffer(mic->stream);
    if (!buffer) return;
    struct spa_buffer *raw = buffer->buffer;
    if (!raw || raw->n_datas != 1 || !raw->datas || !raw->datas[0].data ||
        !raw->datas[0].chunk) {
        mic->failed = 1;
        return;
    }
    struct spa_data *plane = &raw->datas[0];
    uint32_t stride = mic->channels * sizeof(int16_t);
    uint32_t frames = plane->maxsize / stride;
    if (frames > FR_MIC_OUTPUT_FRAMES) frames = FR_MIC_OUTPUT_FRAMES;
    if (buffer->requested && buffer->requested < frames)
        frames = (uint32_t)buffer->requested;
    uint32_t copied = 0;
    uint64_t now = 0, earliest = UINT64_MAX;
    int16_t *output = plane->data;
    memset(output, 0, frames * stride);
    if (!mic->failed && current(mic, &now)) {
        expire(mic, now);
        /* At most four input blocks, with no heap allocation or blocking work. */
        while (copied < frames && mic->count) {
            FrMicBlock *block = &mic->blocks[mic->head];
            if (now >= block->until) {
                expire(mic, now);
                continue;
            }
            uint32_t take = block->frames - block->read;
            if (take > frames - copied) take = frames - copied;
            memcpy(output + copied * mic->channels,
                   block->samples + block->read * mic->channels, take * stride);
            memset(block->samples + block->read * mic->channels, 0, take * stride);
            if (block->until < earliest) earliest = block->until;
            copied += take;
            block->read += take;
            mic->statistics.queued_frames -= take;
            if (block->read == block->frames) retire_front(mic);
        }
    }
    plane->chunk->offset = 0;
    plane->chunk->stride = (int32_t)stride;
    plane->chunk->size = frames * stride;
    buffer->size = frames;
    /* The ORIGINAL deadline and independent terminal fence are checked again
     * immediately before publication to PipeWire, after preparation/copying. */
    if (!current(mic, &now) || now >= earliest || mic->failed) {
        memset(output, 0, frames * stride);
        mic->statistics.expired_frames += copied;
        copied = 0;
    }
    if (pw_stream_queue_buffer(mic->stream, buffer) < 0) {
        mic->failed = 1;
        return;
    }
    mic->statistics.submitted_frames += copied;
    mic->statistics.silent_frames += frames - copied;
}
static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS,
    .state_changed = on_state,
    .param_changed = on_format,
    .process = on_process,
};

void fr_pipewire_mic_init(void) { pw_init(NULL, NULL); }

int fr_pipewire_mic_new(int fd, const char *name, uint32_t channels,
                       FrMicClock clock, const void *context, FrPipewireMic **out) {
    struct ucred peer;
    socklen_t length = sizeof peer;
    if (!out || !name || !clock || !context || channels < 1 || channels > 2 ||
        getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &peer, &length) < 0 ||
        length != sizeof peer || peer.uid != geteuid()) {
        if (fd >= 0) close(fd);
        return -1;
    }
    *out = NULL;
    FrPipewireMic *mic = calloc(1, sizeof *mic);
    if (!mic) { close(fd); return -1; }
    mic->channels = channels;
    mic->clock = clock;
    mic->clock_context = context;
    uint64_t now;
    if (!current(mic, &now)) goto failed;
    mic->loop = pw_loop_new(NULL);
    if (!mic->loop) goto failed;
    /* Bind the loop to this owner thread for its ENTIRE lifetime. PipeWire's
     * loop invoke runs synchronously on a caller when no thread has entered;
     * entering/leaving each poll would allow data-thread callbacks between
     * polls despite RT_PROCESS being unset. Never expose a scoped Rust borrow
     * to that fallback. Keep ownership through stream/context destruction. */
    pw_loop_enter(mic->loop);
    mic->context = pw_context_new(mic->loop, NULL, 0);
    if (!mic->context) goto failed;
    mic->core = pw_context_connect_fd(mic->context, fd, NULL, 0);
    fd = -1; /* PipeWire consumes the connected socket on success AND error. */
    if (!mic->core) goto failed;
    pw_core_add_listener(mic->core, &mic->core_listener, &core_events, mic);
    struct pw_properties *props = pw_properties_new(
        PW_KEY_NODE_NAME, name,
        PW_KEY_NODE_DESCRIPTION, "FrankenRemote microphone",
        PW_KEY_MEDIA_TYPE, "Audio",
        PW_KEY_MEDIA_CATEGORY, "Capture",
        PW_KEY_MEDIA_ROLE, "Communication",
        PW_KEY_MEDIA_CLASS, "Audio/Source",
        PW_KEY_NODE_VIRTUAL, "true",
        PW_KEY_NODE_LATENCY, "480/48000",
        PW_KEY_OBJECT_LINGER, "false",
        "node.autoconnect", "false",
        "stream.dont-remix", "true", NULL);
    if (!props) goto failed;
    mic->stream = pw_stream_new(mic->core, "FrankenRemote microphone", props);
    if (!mic->stream) goto failed;
    pw_stream_add_listener(mic->stream, &mic->stream_listener, &stream_events, mic);
    uint8_t buffer[512];
    struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(buffer, sizeof buffer);
    struct spa_audio_info_raw format = { .format = SPA_AUDIO_FORMAT_S16,
                                         .rate = 48000, .channels = channels };
    format.position[0] = channels == 1 ? SPA_AUDIO_CHANNEL_MONO : SPA_AUDIO_CHANNEL_FL;
    if (channels == 2) format.position[1] = SPA_AUDIO_CHANNEL_FR;
    const struct spa_pod *params[1] = {
        spa_format_audio_raw_build(&builder, SPA_PARAM_EnumFormat, &format)
    };
    if (!params[0] || pw_stream_connect(mic->stream, PW_DIRECTION_OUTPUT, PW_ID_ANY,
            PW_STREAM_FLAG_MAP_BUFFERS, params, 1) < 0) goto failed;
    if (!current(mic, &now)) goto failed;
    *out = mic;
    return 0;
failed:
    if (fd >= 0) close(fd);
    fr_pipewire_mic_free(mic);
    return -1;
}

int fr_pipewire_mic_poll(FrPipewireMic *mic, FrMicPermission permission, void *context) {
    if (!mic || mic->failed || !permission || !context || mic->permission) return -1;
    mic->permission = permission;
    mic->permission_context = context;
    int result = -1;
    uint64_t now;
    if (!current(mic, &now)) { result = 2; goto done; }
    expire(mic, now);
    int iteration = pw_loop_iterate(mic->loop, 0);
    if (mic->stopped) { result = 2; goto done; }
    if (iteration < 0 || mic->failed) goto done;
    if (!current(mic, &now)) { result = 2; goto done; }
    enum pw_stream_state state = pw_stream_get_state(mic->stream, NULL);
    if (state == PW_STREAM_STATE_ERROR || state == PW_STREAM_STATE_UNCONNECTED) goto done;
    mic->statistics.node_id = pw_stream_get_node_id(mic->stream);
    result = (state == PW_STREAM_STATE_PAUSED || state == PW_STREAM_STATE_STREAMING) &&
           mic->statistics.node_id != PW_ID_ANY ? 1 : 0;
done:
    mic->permission = NULL;
    mic->permission_context = NULL;
    return result;
}
int fr_pipewire_mic_submit(FrPipewireMic *mic, const int16_t *pcm,
                          uint32_t frames, uint64_t until) {
    if (!mic || !pcm || frames == 0 || frames > FR_MIC_BLOCK_FRAMES || mic->failed)
        return -1;
    uint64_t now;
    if (!current(mic, &now)) return 3;
    if (now >= until || until - now > FR_MIC_MAX_AGE_US) return 2;
    expire(mic, now);
    if (mic->count == FR_MIC_BLOCKS) return 1;
    uint32_t slot = (mic->head + mic->count) % FR_MIC_BLOCKS;
    FrMicBlock *block = &mic->blocks[slot];
    memcpy(block->samples, pcm, frames * mic->channels * sizeof(int16_t));
    block->frames = frames;
    block->until = until;
    mic->count++;
    mic->statistics.queued_frames += frames;
    return 0;
}
void fr_pipewire_mic_statistics(const FrPipewireMic *mic, FrMicStatistics *out) {
    if (mic && out) *out = mic->statistics;
}
void fr_pipewire_mic_free(FrPipewireMic *mic) {
    if (!mic) return;
    mic->stopped = 1;
    clear(mic);
    if (mic->stream) pw_stream_destroy(mic->stream);
    if (mic->core) pw_core_disconnect(mic->core);
    if (mic->context) pw_context_destroy(mic->context);
    if (mic->loop) {
        pw_loop_leave(mic->loop);
        pw_loop_destroy(mic->loop);
    }
    free(mic);
}
