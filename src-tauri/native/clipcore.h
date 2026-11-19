// clipcore: Clipit's native recording engine.
//
// Capture (Windows Graphics Capture) -> steady frame pacer -> GPU colour
// convert/scale (D3D11 video processor) -> hardware encoder (Media Foundation:
// NVIDIA, AMD and Intel) -> encoded packets handed to the app with their QPC
// time. Frames never leave the GPU. A small AAC encoder is included for saving.
//
// Plain C API so Rust can call it without any glue.

#pragma once
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct cc_engine cc_engine;

// One encoded access unit (Annex B), or one AAC frame. `time_hns` is on the QPC
// clock in 100 ns units (the same clock WASAPI audio and QueryPerformanceCounter use).
typedef void (*cc_packet_fn)(void* user, const uint8_t* data, size_t size, int64_t time_hns, int32_t key);
// A killfeed tap frame: BGR, `w` x `h`, tightly packed.
typedef void (*cc_tap_fn)(void* user, const uint8_t* bgr, uint32_t w, uint32_t h, int64_t time_hns);

typedef struct cc_options {
    intptr_t hwnd;      // capture this window, or
    intptr_t hmonitor;  // this monitor
    uint32_t width;     // output size; 0 = the source's size at start
    uint32_t height;
    uint32_t fps;
    uint32_t kbps;
    uint32_t max_kbps;
    uint32_t codec;     // 0 = H.264, 1 = HEVC
    uint32_t cursor;    // draw the mouse cursor
    uint32_t tap_every; // a tap frame every N output frames (0 = no tap)
    uint32_t tap_w;
    uint32_t tap_h;
    cc_packet_fn on_packet;
    cc_tap_fn on_tap;
    void* user;
} cc_options;

typedef struct cc_stats {
    uint32_t width, height;         // encoded size
    uint32_t src_width, src_height; // last captured size
    uint64_t captured;  // frames from Windows
    uint64_t encoded;   // packets out of the encoder
    uint64_t repeated;  // ticks that re-sent the last frame (the game didn't draw)
    uint64_t dropped;   // ticks skipped (encoder or GPU too busy)
    int64_t last_capture_hns;
    int64_t last_packet_hns;
    int32_t failed;     // the engine stopped on its own; see `error`
    char encoder[128];
    char error[256];
} cc_stats;

// Starts capturing and encoding. Returns NULL (with `err` filled) on failure.
cc_engine* cc_start(const cc_options* o, char* err, size_t errlen);
void cc_get_stats(cc_engine* e, cc_stats* s);
// Stops and frees. No callbacks run after it returns.
void cc_stop(cc_engine* e);

// Encodes interleaved 16-bit PCM to AAC-LC, calling `on_frame` once per AAC
// frame (1024 samples; time_hns = position in the input, key = 1). Returns 0 on
// success. `asc` receives the 2-byte AudioSpecificConfig.
int32_t cc_aac_encode(const int16_t* pcm, size_t frames, uint32_t channels, uint32_t rate, uint32_t kbps,
                      cc_packet_fn on_frame, void* user, uint8_t asc[2], char* err, size_t errlen);

// The same, streaming (long recordings): open, push PCM as it comes, finish.
// Use one encoder from one thread.
typedef struct cc_aac cc_aac;
cc_aac* cc_aac_open(uint32_t channels, uint32_t rate, uint32_t kbps, uint8_t asc[2], char* err, size_t errlen);
int32_t cc_aac_push(cc_aac* a, const int16_t* pcm, size_t frames, cc_packet_fn on_frame, void* user);
int32_t cc_aac_finish(cc_aac* a, cc_packet_fn on_frame, void* user);
void cc_aac_close(cc_aac* a);

#ifdef __cplusplus
}
#endif
