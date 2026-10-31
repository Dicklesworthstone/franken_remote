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
#include <spa/node/io.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define FR_MIC_BLOCKS 4u
#define FR_MIC_BLOCK_FRAMES 961u
#define FR_MIC_OUTPUT_FRAMES 4096u
#define FR_MIC_MAX_AGE_US 100000u
#define FR_MIC_DEMAND_FRAMES 1024u

typedef struct {
    int16_t samples[FR_MIC_BLOCK_FRAMES * 2];
    uint32_t frames, read;
    uint64_t until, start, before;
} FrMicBlock;

typedef struct {
    uint64_t cycle, samples, now_nsec, stamp_nsec;
    uint64_t queued, buffered, delay_frames;
    uint32_t frames, rate_num, rate_denom;
    uint32_t clock_id;
} FrMicGraph;

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
    int demand_mode, graph_started, scheduled_started;
    uint64_t graph_cycle, graph_stamp, graph_samples, scheduled_end;
    uint32_t graph_frames, graph_rate_num, graph_rate_denom;
    uint32_t graph_clock_id;
    const struct spa_io_position *position;
    FrMicDemand demand;
    FrMicBlock blocks[FR_MIC_BLOCKS];
    FrMicStatistics statistics;
};

static void erase_block(FrMicBlock *block) {
    /* Volatile writes keep retired private PCM out of subsequent allocations. */
    volatile int16_t *samples = block->samples;
    for (uint32_t i = 0; i < FR_MIC_BLOCK_FRAMES * 2; i++) samples[i] = 0;
    block->frames = block->read = 0;
    block->until = block->start = block->before = 0;
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
        if (mic->demand_mode) mic->failed = 1;
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
    FrPipewireMic *mic = data;
    if (state == PW_STREAM_STATE_ERROR ||
        (mic->demand_mode && mic->graph_started && state != PW_STREAM_STATE_STREAMING))
        mic->failed = 1;
}
static void on_io(void *data, uint32_t id, void *area, uint32_t size) {
    FrPipewireMic *mic = data;
    if (id != SPA_IO_Position) return;
    const struct spa_io_position *position =
        area && size >= sizeof(struct spa_io_position) ? area : NULL;
    if (mic->demand_mode && mic->graph_started && position != mic->position)
        mic->failed = 1;
    mic->position = position;
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

static int add_u64(uint64_t a, uint64_t b, uint64_t *out) {
    if (UINT64_MAX - a < b) return 0;
    *out = a + b;
    return 1;
}
static int samples_48k(uint64_t ticks, uint32_t num, uint32_t denom,
                        uint64_t *out) {
    if (!num || !denom) return 0;
    __uint128_t scaled = (__uint128_t)ticks * num * 48000u / denom;
    if (scaled > UINT64_MAX) return 0;
    *out = (uint64_t)scaled;
    return 1;
}
/* PipeWire 1.0.5 returns success even before a live clock exists. Only a real
 * STREAMING snapshot is usable. Nothing here uses our submission counters. */
static int graph_time(FrPipewireMic *mic, FrMicGraph *graph) {
    if (pw_stream_get_state(mic->stream, NULL) != PW_STREAM_STATE_STREAMING)
        return mic->graph_started ? -1 : 0;
    if (!mic->position) return mic->graph_started ? -1 : 0;
    /* The public read-only position area identifies a driver replacement.
     * get_time_n deliberately rebases ticks on ID changes, so ticks alone
     * cannot fence a replacement. Read only this aligned scalar atomically;
     * timing itself comes from PipeWire's sequence-protected snapshot. */
    uint32_t clock_id = __atomic_load_n(&mic->position->clock.id, __ATOMIC_ACQUIRE);
    struct pw_time time = { 0 };
    if (pw_stream_get_time_n(mic->stream, &time, sizeof time) < 0 ||
        time.now <= 0 || !time.rate.num || !time.rate.denom || !time.size)
        return mic->graph_started ? -1 : 0;
    if (time.size > FR_MIC_DEMAND_FRAMES) return -1;
    if (clock_id != __atomic_load_n(&mic->position->clock.id, __ATOMIC_ACQUIRE)) return -1;
    uint64_t now = pw_stream_get_nsec(mic->stream);
    if (now < (uint64_t)time.now) return -1;
    uint64_t elapsed;
    if (!samples_48k(now - (uint64_t)time.now, 1, 1000000000u, &elapsed) ||
        elapsed >= time.size * 2u ||
        !samples_48k(time.ticks, time.rate.num, time.rate.denom, &graph->cycle) ||
        !add_u64(graph->cycle, elapsed, &graph->samples)) return -1;
    if (mic->graph_started &&
        (clock_id != mic->graph_clock_id ||
         time.rate.num != mic->graph_rate_num || time.rate.denom != mic->graph_rate_denom ||
         time.size != mic->graph_frames || (uint64_t)time.now < mic->graph_stamp ||
         graph->samples < mic->graph_samples)) return -1;
    uint64_t delay = 0;
    if (time.delay > 0 &&
        !samples_48k((uint64_t)time.delay, time.rate.num, time.rate.denom, &delay))
        return -1;
    graph->delay_frames = delay > elapsed ? delay - elapsed : 0;
    graph->queued = time.queued;
    graph->buffered = time.buffered;
    graph->now_nsec = now;
    graph->stamp_nsec = (uint64_t)time.now;
    graph->frames = (uint32_t)time.size;
    graph->rate_num = time.rate.num;
    graph->rate_denom = time.rate.denom;
    graph->clock_id = clock_id;
    /* At most two requested native buffers and one converter quantum. */
    if (graph->queued > (uint64_t)graph->frames * 2u ||
        graph->buffered > graph->frames || graph->delay_frames > 4800u)
        return -1;
    return 1;
}

typedef struct {
    uint64_t until, before;
    uint32_t offset, frames;
} FrMicPart;

static void process_demand(FrPipewireMic *mic) {
    uint64_t now;
    FrMicGraph graph;
    if (!current(mic, &now)) return;
    int available = graph_time(mic, &graph);
    if (available < 0) { mic->failed = 1; clear(mic); return; }
    if (!available) return;
    /* Multiple process notifications may coalesce onto one graph snapshot.
     * They cannot manufacture another interval or consume PCM twice. */
    if (mic->graph_started && graph.cycle == mic->graph_cycle) return;
    uint64_t expected_cycle;
    if (mic->graph_started &&
        (!add_u64(mic->graph_cycle, mic->graph_frames, &expected_cycle) ||
         graph.cycle != expected_cycle ||
         graph.frames != mic->graph_frames)) {
        mic->failed = 1;
        clear(mic);
        return;
    }
    struct pw_buffer *buffer = pw_stream_dequeue_buffer(mic->stream);
    if (!buffer) { mic->failed = 1; clear(mic); return; }
    struct spa_buffer *raw = buffer->buffer;
    uint32_t stride = mic->channels * sizeof(int16_t);
    if (!raw || raw->n_datas != 1 || !raw->datas || !raw->datas[0].data ||
        !raw->datas[0].chunk || raw->datas[0].maxsize / stride < graph.frames ||
        (buffer->requested && buffer->requested != graph.frames)) {
        mic->failed = 1;
        return;
    }
    struct spa_data *plane = &raw->datas[0];
    int16_t *output = plane->data;
    uint64_t publication_start;
    if (!add_u64(graph.cycle, graph.frames, &publication_start)) {
        mic->failed = 1;
        return;
    }
    uint32_t copied = 0, parts_count = 0;
    FrMicPart parts[FR_MIC_BLOCKS];
    memset(output, 0, graph.frames * stride);
    while (copied < graph.frames && mic->count && !mic->failed) {
        FrMicBlock *block = &mic->blocks[mic->head];
        uint64_t position, wanted;
        if (!add_u64(block->start, block->read, &position) ||
            !add_u64(publication_start, copied, &wanted) || position != wanted ||
            now >= block->until || graph.samples >= block->before) {
            mic->failed = 1;
            break;
        }
        uint32_t take = block->frames - block->read;
        if (take > graph.frames - copied) take = graph.frames - copied;
        parts[parts_count++] = (FrMicPart){ block->until, block->before, copied, take };
        memcpy(output + copied * mic->channels,
               block->samples + block->read * mic->channels, take * stride);
        memset(block->samples + block->read * mic->channels, 0, take * stride);
        copied += take;
        block->read += take;
        mic->statistics.queued_frames -= take;
        if (block->read == block->frames) retire_front(mic);
    }
    if (mic->scheduled_started && copied != graph.frames) mic->failed = 1;
    FrMicGraph fresh;
    if (!current(mic, &now) || graph_time(mic, &fresh) != 1 ||
        fresh.cycle != graph.cycle) mic->failed = 1;
    /* The authority callback may itself be delayed. Recheck it after the
     * native snapshot, then extrapolate that same snapshot with a fresh local
     * clock; no mainloop turn or native device query follows this last guard. */
    if (!mic->failed && !current(mic, &now)) mic->failed = 1;
    if (!mic->failed) {
        uint64_t native_now = pw_stream_get_nsec(mic->stream), elapsed;
        if (native_now < fresh.stamp_nsec ||
            !samples_48k(native_now - fresh.stamp_nsec, 1, 1000000000u, &elapsed) ||
            elapsed >= graph.frames ||
            !add_u64(fresh.cycle, elapsed, &fresh.samples) ||
            fresh.clock_id != __atomic_load_n(&mic->position->clock.id, __ATOMIC_ACQUIRE))
            mic->failed = 1;
    }
    for (uint32_t i = 0; i < parts_count && !mic->failed; i++) {
        FrMicPart *part = &parts[i];
        uint64_t tail, until;
        /* Non-RT callbacks publish for a following cycle. Retain a whole
         * quantum of handoff allowance even when reported graph delay is zero.
         * This is a latency bound, never a consumed-sample clock. */
        uint64_t delay = fresh.delay_frames > graph.frames ? fresh.delay_frames : graph.frames;
        if (now >= part->until || fresh.samples >= part->before ||
            !add_u64(fresh.queued, fresh.buffered, &tail) ||
            !add_u64(tail, delay, &tail) ||
            !add_u64(tail, (uint64_t)part->offset + part->frames, &tail) ||
            tail > 4800u || !add_u64(now, (tail * 1000u + 47u) / 48u, &until) ||
            until >= part->until) mic->failed = 1;
    }
    if (mic->failed || mic->stopped) {
        memset(output, 0, graph.frames * stride);
        mic->statistics.expired_frames += copied;
        copied = 0;
        clear(mic);
    }
    plane->chunk->offset = 0;
    plane->chunk->stride = (int32_t)stride;
    plane->chunk->size = graph.frames * stride;
    buffer->size = graph.frames;
    if (pw_stream_queue_buffer(mic->stream, buffer) < 0) {
        mic->failed = 1;
        clear(mic);
        return;
    }
    mic->statistics.submitted_frames += copied;
    mic->statistics.silent_frames += graph.frames - copied;
    if (mic->failed || mic->stopped) return;
    uint64_t start, end, latest, until;
    if (!add_u64(publication_start, graph.frames, &start) ||
        !add_u64(start, graph.frames, &end) ||
        !add_u64(end, FR_MIC_BLOCK_FRAMES, &latest) ||
        latest <= fresh.samples || latest - fresh.samples > 4800u ||
        !add_u64(now, ((latest - fresh.samples) * 1000u) / 48u, &until) ||
        mic->demand.serial == UINT64_MAX) {
        mic->failed = 1;
        clear(mic);
        return;
    }
    mic->graph_started = 1;
    mic->graph_cycle = graph.cycle;
    mic->graph_stamp = graph.stamp_nsec;
    mic->graph_samples = fresh.samples;
    mic->graph_frames = graph.frames;
    mic->graph_rate_num = graph.rate_num;
    mic->graph_rate_denom = graph.rate_denom;
    mic->graph_clock_id = graph.clock_id;
    mic->demand = (FrMicDemand){ mic->demand.serial + 1u, fresh.samples,
        start, end, until, FR_MIC_BLOCKS - mic->count, graph.frames };
}

static void on_process(void *data) {
    FrPipewireMic *mic = data;
    /* No RT_PROCESS: a process callback is legal only inside our scoped poll. */
    if (!mic->permission) mic->failed = 1;
    if (mic->demand_mode) { process_demand(mic); return; }
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
    .io_changed = on_io,
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
    if (mic->failed) goto done;
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
    if (!mic || !pcm || frames == 0 || frames > FR_MIC_BLOCK_FRAMES || mic->failed ||
        mic->demand_mode)
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
int fr_pipewire_mic_enable_demand(FrPipewireMic *mic) {
    if (!mic || mic->failed || mic->stopped || mic->count) return -1;
    mic->demand_mode = 1;
    return 0;
}
int fr_pipewire_mic_demand(FrPipewireMic *mic, FrMicDemand *out) {
    if (!mic || !out || !mic->demand_mode || mic->failed || mic->stopped) return -1;
    uint64_t now;
    FrMicGraph graph;
    if (!current(mic, &now)) return -1;
    int result = graph_time(mic, &graph);
    if (result < 0 || (mic->graph_started &&
        (now >= mic->demand.until || graph.samples >= mic->demand.output_end))) {
        mic->failed = 1;
        clear(mic);
        return -1;
    }
    if (!result || !mic->graph_started) return 0;
    mic->graph_samples = graph.samples;
    *out = mic->demand;
    out->graph_samples = graph.samples;
    out->available_blocks = FR_MIC_BLOCKS - mic->count;
    return 1;
}
int fr_pipewire_mic_submit_demand(FrPipewireMic *mic, const int16_t *pcm,
                                 uint32_t frames, uint64_t serial,
                                 uint64_t start, uint64_t until) {
    if (!mic || !pcm || !mic->demand_mode || !mic->graph_started ||
        !frames || frames > FR_MIC_BLOCK_FRAMES || mic->failed) return -1;
    uint64_t now, before;
    FrMicGraph graph;
    if (!current(mic, &now)) return 3;
    if (graph_time(mic, &graph) != 1 || serial != mic->demand.serial ||
        start < mic->demand.output_start || start >= mic->demand.output_end ||
        !add_u64(start, frames, &before) || graph.samples >= before ||
        (mic->scheduled_started && start != mic->scheduled_end)) return -1;
    if (now >= until || now >= mic->demand.until || until - now > FR_MIC_MAX_AGE_US)
        return 2;
    if (mic->count == FR_MIC_BLOCKS) return 1;
    uint32_t slot = (mic->head + mic->count) % FR_MIC_BLOCKS;
    FrMicBlock *block = &mic->blocks[slot];
    memcpy(block->samples, pcm, frames * mic->channels * sizeof(int16_t));
    block->frames = frames;
    block->read = 0;
    block->until = until;
    block->start = start;
    block->before = before;
    mic->count++;
    mic->statistics.queued_frames += frames;
    mic->scheduled_started = 1;
    mic->scheduled_end = before;
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
