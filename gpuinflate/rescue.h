/* gzrescue - recover what a damaged gzip file still holds (CPU only).
 *
 * gzip stops at the first damaged byte, and Deflate often decodes damage as plausible garbage
 * that only the trailer's CRC reveals. gr_rescue finds every block header in the file, decodes
 * block by block (a decode is right when it lands exactly on the next header and holds the
 * encoder's regular number of symbols), keeps the damaged blocks flagged, restarts after them,
 * and marks the bytes that copied from a damaged block as unknown. The output comes in order
 * through the sink with a mask: per byte, GR_EXACT, GR_SUSPECT (inside a damaged block: right
 * from its start, wrong from somewhere inside; when the damage is a run of zeros or 0xFF bytes,
 * the block is split there and what came before it is exact) or GR_UNKNOWN (written as the fill
 * byte). A member of one piece whose CRC fails though it decoded in step is flagged the same way. A
 * damaged block's true length is taken from the trailer when it is the only one, so the data
 * after it keeps its offsets. See research/recovery/README.md for what this recovers. */
#ifndef GZRESCUE_H
#define GZRESCUE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#if defined(GR_BUILD)
#define GR_API __declspec(dllexport)
#elif defined(GR_STATIC)
#define GR_API
#else
#define GR_API __declspec(dllimport)
#endif
#elif defined(GR_BUILD) && defined(__GNUC__)
#define GR_API __attribute__((visibility("default"))) /* the only symbols exported with -fvisibility=hidden */
#else
#define GR_API
#endif

#define GR_ABI_VERSION 4

enum { GR_OK = 0, GR_ERR_NO_DEFLATE = 1, GR_ERR_ABORTED = 2, GR_ERR_ARG = 3, GR_ERR_MEMORY = 4, GR_ERR_TOO_BIG = 5, GR_ERR_INTERNAL = 6 };
/* GR_INFERRED: a byte that copied from data lost with a damaged block, its value inferred from
 * every place that copy appears and the file's own statistics; likely right, not certain */
enum { GR_UNKNOWN = 0, GR_EXACT = 1, GR_SUSPECT = 2, GR_INFERRED = 3 };

typedef struct {
    uint32_t size;    /* sizeof(gr_options) */
    uint8_t fill;     /* written where a byte is unknown */
    uint8_t no_infer; /* 1: leave the bytes copied from lost data unknown, do not infer them */
    uint8_t cr_lost;  /* a Windows-to-Unix ASCII transfer removed the CR of every CR LF: 0 tell from the
                       * data, 1 it did (a ZIP entry, too short to tell, of a file that did), 2 never */
    uint8_t crs_expected; /* with cr_lost 1, a raw stream: how many CRs it lost, when the caller knows
                           * (a ZIP entry's size against the room it has): 1-8; 255 none; 0 unknown */
    uint64_t max_output;  /* the rescue stops, returning GR_ERR_TOO_BIG and no result, when its output
                           * would be larger than this many bytes (a gzip bomb): 0 no limit */
    uint64_t cr_budget;   /* with cr_lost: input bytes the search for the lost CRs may decode; 0: 1 GiB */
} gr_options;

typedef struct {
    uint64_t out_offset, length; /* in the output */
    uint64_t decoded, known;     /* bytes the decode produced, of which not unknown */
    uint64_t in_byte_start, in_byte_end;
    int32_t suspect;             /* 1: a damaged block */
    int32_t status;              /* how the decode ended (gi::Status) */
    uint32_t blocks, symbols;
    const char* why;             /* suspect: how the damage showed; else "" */
} gr_piece;

typedef struct {
    uint64_t input_size, output_size;
    uint64_t exact_bytes, suspect_bytes, unknown_bytes;
    uint32_t headers, pieces, damaged, members;
    int32_t crc_check;  /* 1 ok, 0 mismatch, -1 not checkable */
    int64_t length_off; /* output length minus the trailer's, when every block stayed in step; else 0 */
    int32_t truncated;  /* the file ends inside the stream */
    int32_t header_damaged;
    uint64_t input_end; /* byte after the last decoded block of the first member (a ZIP's data descriptor follows it) */
    uint64_t inferred_bytes;
    int32_t ascii_undone; /* the input had gone through an ASCII-mode transfer (LF to CR LF), undone */
    uint32_t cr_restored; /* CRs put back where a CR LF to LF transfer had removed them */
    uint64_t cr_decoded;  /* input bytes the search for them decoded (a caller's budget for a whole file) */
} gr_stats;

/* Receives the output in order. Return nonzero to stop. */
typedef int (*gr_sink)(void* user, const uint8_t* data, const uint8_t* mask, size_t len);

typedef struct gr_result gr_result;

GR_API uint32_t gr_abi_version(void);

/* Recovers the gzip data in in[0..size) into the sink. On success (or GR_ERR_ABORTED) *result
 * holds the statistics, every piece and the notes; free it with gr_free. */
GR_API int gr_rescue(const uint8_t* in, uint64_t size, const gr_options* opt, gr_sink sink, void* user, gr_result** result);

/* The same for a raw Deflate stream (a ZIP entry): the stream starts at in[0]; in may run on
 * past it (a data descriptor, when the compressed size is unknown). expected_size is the
 * uncompressed size, or UINT64_MAX when unknown; has_crc says whether expected_crc holds the
 * CRC-32 of the original. They stand in for the gzip trailer. */
GR_API int gr_rescue_raw(const uint8_t* in, uint64_t size, uint64_t expected_size, int has_crc, uint32_t expected_crc,
                         const gr_options* opt, gr_sink sink, void* user, gr_result** result);
GR_API const gr_stats* gr_result_stats(const gr_result* r);
GR_API uint32_t gr_result_pieces(const gr_result* r, const gr_piece** pieces);
GR_API uint32_t gr_result_notes(const gr_result* r, const char* const** notes);
GR_API void gr_free(gr_result* r);

#ifdef __cplusplus
}
#endif

#endif
