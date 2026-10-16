/* gpuinflate - decompress one raw Deflate stream (RFC 1951) in parallel on an NVIDIA GPU.
 *
 * The stream is processed in segments of compressed input, each decoded speculatively by
 * thousands of GPU threads at once; the output reaches the caller in order, one segment at a
 * time, so streams of any size work in bounded memory. Corrupt or malicious input makes
 * gi_inflate return an error; it never reads or writes outside its buffers.
 *
 * A context is not thread-safe: use one per thread. */
#ifndef GPUINFLATE_H
#define GPUINFLATE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#if defined(GI_BUILD)
#define GI_API __declspec(dllexport)
#else
#define GI_API __declspec(dllimport)
#endif
#else
#define GI_API
#endif

#define GI_ABI_VERSION 1

enum {
    GI_OK = 0,
    GI_ERR_DATA = 1,      /* the stream is not valid Deflate */
    GI_ERR_TRUNCATED = 2, /* the input ends before the stream's last block does */
    GI_ERR_ABORTED = 3,   /* the sink returned nonzero */
    GI_ERR_GPU = 4,       /* no usable GPU, out of GPU memory, or a CUDA error: decode on the CPU */
    GI_ERR_ARG = 5        /* a bad argument */
};

typedef struct gi_ctx gi_ctx;

typedef struct {
    uint32_t size;           /* sizeof(gi_config) */
    int32_t device;          /* CUDA device ordinal */
    uint32_t segment_out_mb; /* most output per segment, MB (0: from the GPU's free memory, at most 256) */
    uint32_t segment_in_kb;  /* compressed input per segment, KB (0: from the stream's ratio) */
} gi_config;

typedef struct {
    uint64_t in_used;  /* bytes of input the stream took, through its last block (byte-aligned) */
    uint64_t out_size; /* bytes passed to the sink */
    uint32_t segments;
    uint32_t rounds;   /* decode rounds, all segments */
    uint64_t jobs;     /* decoders started, all segments */
    double seconds;    /* the whole call */
    double upload_s, find_s, decode_s, resolve_s, download_s; /* GPU time by stage */
} gi_stats;

/* Receives the output in order, from a thread of the library's own, never two calls at once.
 * Return nonzero to stop. */
typedef int (*gi_sink)(void* user, const uint8_t* data, size_t len);

GI_API uint32_t gi_abi_version(void);

/* Allocates the GPU and pinned host buffers. On failure returns an error code and describes
 * it in err (when given). */
GI_API int gi_create(const gi_config* cfg, gi_ctx** ctx, char* err, size_t err_len);

/* Decompresses the raw Deflate stream at the start of in[0..in_size). Bytes after the stream's
 * last block are allowed (a gzip trailer, the next member) and not read past what the stream
 * needs; stats->in_used says where the stream ended. out_hint: the expected output size, or 0
 * when unknown (it only tunes the first segment). stats may be NULL. */
GI_API int gi_inflate(gi_ctx* ctx, const uint8_t* in, uint64_t in_size, uint64_t out_hint, gi_sink sink, void* user,
                      gi_stats* stats);

/* What went wrong in the last call that failed */
GI_API const char* gi_last_error(const gi_ctx* ctx);

/* The GPU's name, for reports */
GI_API const char* gi_device_name(const gi_ctx* ctx);

GI_API void gi_destroy(gi_ctx* ctx);

#ifdef __cplusplus
}
#endif

#endif
