#ifndef FUNKOT_H
#define FUNKOT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct FunkotEngine FunkotEngine; /* opaque */

/*
 * A handle has one owner. Do not call any function concurrently on the same
 * handle: render is mutually exclusive with poll_event, stop, and free, and
 * free is valid only once. The paths and cache_dir passed to new are copied
 * during creation; they need only remain valid for that call.
 *
 * render is the only handle operation suitable for an audio callback. Call
 * poll_event, stop, and free from a control thread. stop signals the loader
 * and detaches any in-flight preparation; it does not wait for that work to
 * complete. free implies stop and invalidates the handle immediately.
 */

typedef struct FunkotOptions {
    double   rate;               /* speed-up factor, default 1.10 */
    int32_t  pitch_shift;        /* 0 = preserve pitch, 1 = shift */
    uint32_t fade_bars;          /* default 4 */
    float    highpass_hz;        /* default 300.0; mid/high-pass cutoff */
    int32_t  gain_normalize;     /* bool */
    int32_t  random;             /* bool */
    int32_t  loop_playlist;      /* bool */
    uint32_t output_sample_rate; /* e.g. 48000 */
    const char* cache_dir;       /* UTF-8, may be NULL -> "funkot-cache" */
} FunkotOptions;

typedef enum FunkotEventType {
    FUNKOT_EVENT_NONE = 0,
    FUNKOT_EVENT_TRACK_STARTED = 1,
    FUNKOT_EVENT_TRANSITION_STARTED = 2,
    FUNKOT_EVENT_TRACK_FAILED = 3,
    FUNKOT_EVENT_FINISHED = 4,
} FunkotEventType;

typedef struct FunkotEvent {
    FunkotEventType type;
    int32_t track_index;      /* TRACK_STARTED: playlist index, else -1 */
    char    path[512];        /* UTF-8, NUL-terminated, may be truncated; "" if n/a */
    char    detail[256];      /* TRANSITION_STARTED: source path; TRACK_FAILED: message; else "" */
} FunkotEvent;

/* Fill `options` with defaults. */
void funkot_options_default(FunkotOptions* options);

/*
 * Create an engine for a playlist of UTF-8 file paths.
 * Returns NULL on failure; if err/err_len given, writes a NUL-terminated UTF-8 message.
 *
 * Empty playlist (n_paths == 0): creation succeeds. The first render returns 0 and a
 * FINISHED event is queued (matches the core loader Exhausted behaviour).
 */
FunkotEngine* funkot_engine_new(const FunkotOptions* options,
                                const char* const* paths, size_t n_paths,
                                char* err, size_t err_len);

/*
 * Pull interleaved stereo f32 audio. Returns frames written (<= max_frames);
 * 0 means playback finished. Never blocks.
 */
size_t funkot_engine_render(FunkotEngine* engine, float* out, size_t max_frames);

/* Pop one pending event. Returns 1 and fills *event, or 0 if none pending. */
int32_t funkot_engine_poll_event(FunkotEngine* engine, FunkotEvent* event);

/* Stop playback; see the handle ownership and threading contract above. */
void funkot_engine_stop(FunkotEngine* engine);

/* Destroy the engine (implies stop). NULL-safe; no later call may use it. */
void funkot_engine_free(FunkotEngine* engine);

#ifdef __cplusplus
}
#endif

#endif /* FUNKOT_H */
