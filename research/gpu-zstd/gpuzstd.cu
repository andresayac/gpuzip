// gpuzstd.cu - the GPU half of a zstd compressor: match finding and an optimal parse on the
// GPU, then zstd's own entropy coder (libzstd's ZSTD_compressSequences) on the CPU. The output
// is a standard zstd file, one frame per 128 MB chunk (the window).
//
//   gpuzstd <file> [-o out.zst] [--chunk-mb 128] [--passes 2] [--sample 8] [--orders 4] [--pred 4] [--baseline]
//
// Match finding: see match.cu (stable radix sorts of (prefix, position) records, one per order:
// the first 4 bytes, the first 8, hashes of the first 16 and 32; three candidates per
// position). Parse: one GPU thread per 4 KB segment runs the shortest-path parse zstd's
// btultra runs, priced in bits from code statistics: edges are a literal, a match at every
// length from 3 up (candidates and the three repeat offsets), and a match of 64+ bytes is taken
// whole, so the dynamic programme needs only a ring of 64 nodes per thread (zstd's own
// "sufficient length" shortcut). A long match may run past the segment's end; the host trims
// the next segment's parse to where it ended. Pass 1 prices with flat estimates and collects
// code statistics on a sample of the segments; pass 2 prices with them. Each thread then walks
// its path back and writes its segment's sequences; the host joins the segments (literals carry
// over) and hands the sequences to libzstd, which blocks and entropy-codes them. --baseline also
// runs libzstd's own level 19 with long-distance matching on the same window, for comparison.
#define ZSTD_STATIC_LINKING_ONLY
#include <zstd.h>

#include <cub/cub.cuh>
#include <cuda_runtime.h>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

#define CK(x)                                                                                  \
    do {                                                                                       \
        cudaError_t e_ = (x);                                                                  \
        if (e_ != cudaSuccess) {                                                               \
            fprintf(stderr, "%s:%d %s: %s\n", __FILE__, __LINE__, #x, cudaGetErrorString(e_)); \
            exit(1);                                                                           \
        }                                                                                      \
    } while (0)

// ---------------------------------------------------------------- match finding (as match.cu)

constexpr uint32_t CAP = 64;  // candidates are extended this far; the parse extends the rest on demand
constexpr int KC = 3;         // candidates kept per position
constexpr uint32_t PAD = 32;  // zero bytes after the data on the device

struct Cand {
    uint32_t off;  // 0: no match
    uint32_t len;
};

__device__ __forceinline__ uint32_t be32(const uint8_t* p) {
    return (uint32_t)p[0] << 24 | (uint32_t)p[1] << 16 | (uint32_t)p[2] << 8 | p[3];
}

__device__ __forceinline__ uint64_t mix(uint64_t h, uint64_t v) {
    h ^= v;
    h *= 0x9E3779B97F4A7C15ull;
    return h ^ (h >> 29);
}

// Keys for one sort order: the first 4 bytes, the first 8, or a hash of the first 16 or 32.
// With H32 the 8-byte key and the hashes are folded to 32 bits: the sort takes half the passes,
// and the few false groups that collisions make are rejected when the bytes are compared.
template <int BYTES, bool H32>
__global__ void k_keys(const uint8_t* d, uint32_t n, void* key, uint32_t* pos) {
    uint32_t i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= n) return;
    if constexpr (BYTES == 4) {
        ((uint32_t*)key)[i] = be32(d + i);
    } else {
        uint64_t h;
        if constexpr (BYTES == 8) {
            h = (uint64_t)be32(d + i) << 32 | be32(d + i + 4);
            if (H32) h = mix(0x12345678ull, h);
        } else {
            uint64_t w[BYTES / 8];
            memcpy(w, d + i, BYTES);
            h = 0x12345678ull;
            for (int k = 0; k < BYTES / 8; k++) h = mix(h, w[k]);
        }
        if constexpr (H32) ((uint32_t*)key)[i] = (uint32_t)(h ^ (h >> 32));
        else ((uint64_t*)key)[i] = h;
    }
    pos[i] = i;
}

// bytes in common from a and b, up to lim
__device__ __forceinline__ uint32_t match_len(const uint8_t* d, uint32_t a, uint32_t b, uint32_t lim) {
    uint32_t len = 0;
    while (len + 8 <= lim) {
        uint64_t x, y;
        memcpy(&x, d + a + len, 8);
        memcpy(&y, d + b + len, 8);
        if (x != y) return len + (__ffsll((long long)(x ^ y)) - 1) / 8;
        len += 8;
    }
    while (len < lim && d[a + len] == d[b + len]) len++;
    return len;
}

// Per-position arrays (candidates, path records) are laid out segment-interleaved: entry
// (i % seg) * nseg + i / seg, so the 32 threads of a warp, parsing 32 consecutive segments at
// the same offset, read 32 consecutive entries.
__device__ __forceinline__ size_t sidx(uint32_t i, uint32_t seg, uint32_t nseg) {
    return (size_t)(i % seg) * nseg + i / seg;
}

// For the position at rank r in a sorted order: its nearest predecessors with the same key are
// its nearest earlier occurrences. Writes the nearest match to slot_near (if >= 0) and the
// longest found to slot_best, when longer than what is there.
template <class KEY>
__global__ void k_candidates(const uint8_t* d, uint32_t n, uint32_t seg, uint32_t nseg, const KEY* keys,
                             const uint32_t* pos, int npred, int minlen, int slot_near, int slot_best, Cand* cand) {
    uint32_t r = blockIdx.x * blockDim.x + threadIdx.x;
    if (r >= n) return;
    uint32_t i = pos[r];
    KEY k = keys[r];
    uint32_t best_len = 0, best_off = 0;
    Cand* c = cand + sidx(i, seg, nseg) * KC;
    // orders run longest first: a position that already has a match of CAP+ bytes is inside a
    // long match, where the parse never stops to look at anything shorter
    for (int s = 0; s < KC; s++)
        if (c[s].len >= CAP) return;
    for (int p = 1; p <= npred && (uint32_t)p <= r; p++) {
        if (keys[r - p] != k) break;
        uint32_t j = pos[r - p];  // j < i: the sort is stable and the input was in position order
        uint32_t len = match_len(d, j, i, min(CAP, n - i));
        if (p == 1 && slot_near >= 0 && len >= 3) c[slot_near] = Cand{i - j, len};
        if (len > best_len) {
            best_len = len;
            best_off = i - j;
            if (len >= CAP) break;  // the cap: no predecessor can do better
        }
    }
    if (best_len >= (uint32_t)minlen && best_len > c[slot_best].len) c[slot_best] = Cand{best_off, best_len};
}

// ---------------------------------------------------------------- the parse

constexpr uint32_t RING = 64;          // nodes kept: a match shorter than this goes through the dynamic programme
constexpr uint32_t SUFF = RING;        // a match this long is taken whole
constexpr uint32_t MAX_MATCH = 65535;  // longer matches are emitted in pieces
// a thread parses `seg` positions (an option, default 4096); a segment holds at most seg / 3 + 1 sequences

struct Seq {  // ZSTD_Sequence
    uint32_t offset, litLength, matchLength, rep;
};

struct Rec {  // how a position was reached on the best path: a match, or a literal (len 0)
    uint32_t off;
    uint16_t len;
    uint16_t pad;
};

struct Stats {
    unsigned lit[256], ll[36], ml[53], off[32];
};

// prices in bits: literals by byte; literal-length and offset codes with their extra bits;
// match lengths 3..63 by length, longer ones by code
__constant__ float c_lit[256];
__constant__ float c_llp[36];
__constant__ float c_mlp[64];
__constant__ float c_mlc[53];
__constant__ float c_offp[32];

__constant__ uint8_t c_llcode[64] = {0,  1,  2,  3,  4,  5,  6,  7,  8,  9,  10, 11, 12, 13, 14, 15, 16, 16, 17, 17, 18, 18,
                                     19, 19, 20, 20, 20, 20, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23,
                                     23, 23, 23, 23, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24};
static const uint8_t h_llcode[64] = {0,  1,  2,  3,  4,  5,  6,  7,  8,  9,  10, 11, 12, 13, 14, 15, 16, 16, 17, 17, 18, 18,
                                     19, 19, 20, 20, 20, 20, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23,
                                     23, 23, 23, 23, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24};
static const uint8_t LL_BITS[36] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16};
static const uint8_t ML_BITS[53] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2,
                                    2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16};

__host__ __device__ __forceinline__ int highbit(uint32_t v) {
#if defined(__CUDA_ARCH__)
    return 31 - __clz(v);
#else
    int b = 0;
    while (v >>= 1) b++;
    return b;
#endif
}

__host__ __device__ __forceinline__ int ll_code(uint32_t ll) {
#if defined(__CUDA_ARCH__)
    return ll > 63 ? min(highbit(ll) + 19, 35) : c_llcode[ll];
#else
    return ll > 63 ? std::min(highbit(ll) + 19, 35) : h_llcode[ll];
#endif
}

__host__ __device__ __forceinline__ int ml_code(uint32_t ml) {
    uint32_t b = ml - 3;
    if (b > 127) {
        int c = highbit(b) + 36;
        return c > 52 ? 52 : c;
    }
    if (b < 32) return (int)b;
    if (b < 40) return 32 + (b - 32) / 2;
    if (b < 48) return 36 + (b - 40) / 4;
    if (b < 64) return 38 + (b - 48) / 8;
    if (b < 96) return 40 + (b - 64) / 16;
    return 42;
}

__host__ __device__ __forceinline__ uint32_t off_base(uint32_t off, const uint32_t* reps) {
    return off == reps[0] ? 1 : off == reps[1] ? 2 : off == reps[2] ? 3 : off + 3;
}

__host__ __device__ __forceinline__ void push_rep(uint32_t* out, const uint32_t* reps, uint32_t off) {
    if (off == reps[0]) {
        out[0] = reps[0], out[1] = reps[1], out[2] = reps[2];
    } else if (off == reps[1]) {
        out[0] = reps[1], out[1] = reps[0], out[2] = reps[2];
    } else {
        out[0] = off, out[1] = reps[0], out[2] = reps[1];
    }
}

constexpr int PARSE_WARPS = 8;  // one warp parses one segment; a block holds this many
constexpr unsigned FULL = 0xffffffffu;

// Bytes in common from a and b, up to lim, found by the whole warp: lane k compares the 8 bytes
// at offset 8k, 256 bytes per round. Uniform result.
__device__ __forceinline__ uint32_t wmatch(const uint8_t* d, uint32_t a, uint32_t b, uint32_t lim, int lane) {
    uint32_t len = 0;
    while (len < lim) {
        uint32_t o = len + lane * 8;
        uint64_t x = 0, y = 1;  // lanes past lim report a difference
        if (o < lim) {
            memcpy(&x, d + a + o, 8);  // the data is padded, so the tail read stays in bounds
            memcpy(&y, d + b + o, 8);
            if (o + 8 > lim) {
                uint64_t mask = (1ull << (8 * (lim - o))) - 1;
                x &= mask, y &= mask;
            }
        }
        unsigned neq = __ballot_sync(FULL, x != y);
        if (neq) {
            int first = __ffs(neq) - 1;
            uint64_t xf = __shfl_sync(FULL, x, first), yf = __shfl_sync(FULL, y, first);
            uint32_t within = xf == yf ? 8 : (__ffsll((long long)(xf ^ yf)) - 1) / 8;
            return min(lim, len + first * 8 + within);
        }
        len += 256;
    }
    return lim;
}

// One warp per segment. The ring of RING nodes lives in shared memory, one array per field.
// The dynamic programme's state is warp-uniform; the lanes share the work of each position:
// comparing bytes 256 at a time, and relaxing one match length each. Writes the segment's
// sequences (forward order) to seqs + t * maxseq, their count, where the parse ended (at the
// segment's end, or further when its last match ran past it) and the literals after its last
// match. With `stats`, also counts the codes used (pass 1).
__global__ void __launch_bounds__(PARSE_WARPS * 32)
k_parse(const uint8_t* d, uint32_t n, uint32_t seg, uint32_t nseg, uint32_t maxseq, uint32_t stride, const Cand* cand,
        Rec* rec, Seq* seqs, uint32_t* counts, uint32_t* ends, uint32_t* trailing, Stats* stats) {
    __shared__ float s_cost[PARSE_WARPS][RING];
    __shared__ uint32_t s_off[PARSE_WARPS][RING], s_ll[PARSE_WARPS][RING], s_r0[PARSE_WARPS][RING],
        s_r1[PARSE_WARPS][RING], s_r2[PARSE_WARPS][RING];
    __shared__ float p_lit[256], p_llp[36], p_mlp[64], p_mlc[53], p_offp[32];
    __shared__ unsigned sh_lit[256], sh_ll[36], sh_ml[53], sh_off[32];
    for (int k = threadIdx.x; k < 256; k += blockDim.x) p_lit[k] = c_lit[k], sh_lit[k] = 0;
    for (int k = threadIdx.x; k < 36; k += blockDim.x) p_llp[k] = c_llp[k], sh_ll[k] = 0;
    for (int k = threadIdx.x; k < 64; k += blockDim.x) p_mlp[k] = c_mlp[k];
    for (int k = threadIdx.x; k < 53; k += blockDim.x) p_mlc[k] = c_mlc[k], sh_ml[k] = 0;
    for (int k = threadIdx.x; k < 32; k += blockDim.x) p_offp[k] = c_offp[k], sh_off[k] = 0;
    __syncthreads();
    int w = threadIdx.x >> 5, lane = threadIdx.x & 31;
    uint32_t t = blockIdx.x * PARSE_WARPS + w;
    size_t s0_ = (size_t)t * stride * seg;
    if (s0_ < n) {
        float* cost = s_cost[w];
        uint32_t *off_ = s_off[w], *ll_ = s_ll[w], *r0_ = s_r0[w], *r1_ = s_r1[w], *r2_ = s_r2[w];
        auto relax = [&](uint32_t j, float c, uint32_t o, uint32_t len, uint32_t litrun, uint32_t r0, uint32_t r1, uint32_t r2) {
            uint32_t k = j & (RING - 1);
            if (c < cost[k]) {
                cost[k] = c;
                off_[k] = o;
                ll_[k] = (len & 0xffffu) | (min(litrun, 65535u) << 16);
                r0_[k] = r0, r1_[k] = r1, r2_[k] = r2;
            }
        };
        uint32_t s0 = (uint32_t)s0_, s1 = min(n, s0 + seg);
        for (uint32_t k = lane; k < RING; k += 32) cost[k] = INFINITY;
        __syncwarp();
        if (lane == 0) relax(s0, 0.0f, 0, 0, 0, 1, 4, 8);
        __syncwarp();
        uint32_t i = s0;
        while (i < s1) {
            uint32_t k0 = i & (RING - 1);
            float ccost = cost[k0];
            uint32_t coff = off_[k0], cll = ll_[k0], cr0 = r0_[k0], cr1 = r1_[k0], cr2 = r2_[k0];
            uint32_t clen = cll & 0xffffu, clit = cll >> 16;
            __syncwarp();
            if (lane == 0) {
                cost[k0] = INFINITY;
                rec[sidx(i, seg, nseg)] = Rec{coff, (uint16_t)clen, 0};
                relax(i + 1, ccost + p_lit[d[i]], 0, 0, clit + 1, cr0, cr1, cr2);
            }
            __syncwarp();
            uint32_t room = s1 - i;                      // a short match ends inside the segment
            uint32_t room_long = min(n - i, MAX_MATCH);  // a long one may run past it
            if (room_long >= 3) {
                float llp = p_llp[ll_code(clit)];
                float best_long = INFINITY;
                uint32_t bl_off = 0, bl_len = 0;
                const Cand* cp = cand + sidx(i, seg, nseg) * KC;
                Cand c0 = cp[0], c1 = cp[1], c2 = cp[2];
                uint32_t reps[3] = {cr0, cr1, cr2};
                // six sources: the three repeat offsets, then the three candidates
                for (int src = 0; src < 6; src++) {
                    uint32_t off, L;
                    if (src < 3) {
                        off = reps[src];
                        // most repeat offsets fail within the first bytes: check before the warp compares
                        if (off > i || d[i] != d[i - off] || d[i + 1] != d[i + 1 - off] || d[i + 2] != d[i + 2 - off]) continue;
                        L = wmatch(d, i - off, i, room_long, lane);
                    } else {
                        Cand c = src == 3 ? c0 : src == 4 ? c1 : c2;
                        off = c.off;
                        if (c.len < 3 || off == cr0 || off == cr1 || off == cr2) continue;
                        L = c.len >= CAP ? wmatch(d, i - off, i, room_long, lane) : c.len;
                    }
                    if (L < 3) continue;
                    uint32_t ob = off_base(off, reps);
                    float base = ccost + llp + p_offp[highbit(ob)];
                    if (L >= SUFF) {
                        float c = base + p_mlc[ml_code(L)];
                        if (c < best_long) {
                            best_long = c;
                            bl_off = off;
                            bl_len = L;
                        }
                        continue;
                    }
                    L = min(L, room);
                    if (L < 3) continue;
                    uint32_t nr[3];
                    push_rep(nr, reps, off);
                    for (uint32_t l = 3 + lane; l <= L; l += 32) relax(i + l, base + p_mlp[l], off, l, 0, nr[0], nr[1], nr[2]);
                    __syncwarp();
                }
                if (best_long < INFINITY) {  // taken whole: the path is committed up to here
                    for (uint32_t k = lane; k < RING; k += 32) cost[k] = INFINITY;
                    __syncwarp();
                    if (lane == 0) {
                        uint32_t nr[3];
                        push_rep(nr, reps, bl_off);
                        relax(i + bl_len, best_long, bl_off, bl_len, 0, nr[0], nr[1], nr[2]);
                    }
                    __syncwarp();
                    i += bl_len;
                    continue;
                }
            }
            i++;
        }
        // walk the best path back from where the parse ended, then put the sequences in order
        uint32_t e = i;
        if (lane == 0) {
            uint32_t ke = e & (RING - 1);
            Seq* out = seqs + (size_t)t * maxseq;
            uint32_t cnt = 0, lits = 0, pos = e;
            uint32_t off = off_[ke], len = ll_[ke] & 0xffffu;
            int last = -1;
            uint32_t trail = 0;
            while (pos > s0) {
                if (len == 0) {
                    pos--;
                    lits++;
                } else {
                    if (last < 0) trail = lits;
                    else out[last].litLength = lits;
                    out[cnt] = Seq{off, 0, len, 0};
                    last = (int)cnt++;
                    lits = 0;
                    pos -= len;
                }
                Rec r = rec[sidx(pos, seg, nseg)];
                off = r.off;
                len = r.len;
            }
            if (last < 0) trail = lits;
            else out[last].litLength = lits;
            for (uint32_t a = 0, b = cnt ? cnt - 1 : 0; a < b; a++, b--) {
                Seq x = out[a];
                out[a] = out[b];
                out[b] = x;
            }
            counts[t] = cnt;
            ends[t] = e;
            trailing[t] = trail;
            if (stats) {  // the codes this parse used, with repeat offsets as libzstd will find them
                uint32_t reps[3] = {1, 4, 8};
                uint32_t p = s0;
                for (uint32_t q = 0; q < cnt; q++) {
                    Seq sq = out[q];
                    for (uint32_t k = 0; k < sq.litLength; k++) atomicAdd(&sh_lit[d[p + k]], 1u);
                    p += sq.litLength;
                    atomicAdd(&sh_ll[ll_code(sq.litLength)], 1u);
                    atomicAdd(&sh_ml[ml_code(sq.matchLength)], 1u);
                    atomicAdd(&sh_off[highbit(off_base(sq.offset, reps))], 1u);
                    uint32_t nr[3];
                    push_rep(nr, reps, sq.offset);
                    reps[0] = nr[0], reps[1] = nr[1], reps[2] = nr[2];
                    p += sq.matchLength;
                }
                for (uint32_t k = 0; k < trail; k++) atomicAdd(&sh_lit[d[p + k]], 1u);
            }
        }
    }
    if (stats) {
        __syncthreads();
        for (int k = threadIdx.x; k < 256; k += blockDim.x) atomicAdd(&stats->lit[k], sh_lit[k]);
        for (int k = threadIdx.x; k < 36; k += blockDim.x) atomicAdd(&stats->ll[k], sh_ll[k]);
        for (int k = threadIdx.x; k < 53; k += blockDim.x) atomicAdd(&stats->ml[k], sh_ml[k]);
        for (int k = threadIdx.x; k < 32; k += blockDim.x) atomicAdd(&stats->off[k], sh_off[k]);
    }
}

// ---------------------------------------------------------------- host

using clk = std::chrono::steady_clock;
static double since(clk::time_point t) { return std::chrono::duration<double>(clk::now() - t).count(); }
static float ms(cudaEvent_t a, cudaEvent_t b) {
    float t;
    CK(cudaEventElapsedTime(&t, a, b));
    return t;
}

// prices to constant memory from per-code tables (bits)
static void set_prices(const float* lit, const float* ll, const float* ml, const float* off) {
    float llp[36], mlp[64], mlc[53], offp[32];
    for (int c = 0; c < 36; c++) llp[c] = ll[c] + LL_BITS[c];
    for (int c = 0; c < 53; c++) mlc[c] = ml[c] + ML_BITS[c];
    for (int l = 0; l < 64; l++) mlp[l] = l < 3 ? 1e9f : mlc[ml_code(l)];
    for (int c = 0; c < 32; c++) offp[c] = off[c] + c;
    CK(cudaMemcpyToSymbol(c_lit, lit, sizeof(float) * 256));
    CK(cudaMemcpyToSymbol(c_llp, llp, sizeof llp));
    CK(cudaMemcpyToSymbol(c_mlp, mlp, sizeof mlp));
    CK(cudaMemcpyToSymbol(c_mlc, mlc, sizeof mlc));
    CK(cudaMemcpyToSymbol(c_offp, offp, sizeof offp));
}

template <int N>
static void price_table(const unsigned* h, float floor_, float cap, float* out) {
    double total = N;
    for (int i = 0; i < N; i++) total += h[i];
    for (int i = 0; i < N; i++) out[i] = (float)std::min<double>(cap, std::max<double>(floor_, -std::log2((h[i] + 1.0) / total)));
}

struct ZstdCtx {
    ZSTD_CCtx* c = ZSTD_createCCtx();
    ZstdCtx(int level, int window_log, bool sequences) {
        ZSTD_CCtx_setParameter(c, ZSTD_c_compressionLevel, level);
        ZSTD_CCtx_setParameter(c, ZSTD_c_windowLog, window_log);
        if (sequences) {
            ZSTD_CCtx_setParameter(c, ZSTD_c_minMatch, 3);
            ZSTD_CCtx_setParameter(c, ZSTD_c_blockDelimiters, ZSTD_sf_noBlockDelimiters);
            ZSTD_CCtx_setParameter(c, ZSTD_c_validateSequences, 1);
        } else {
            ZSTD_CCtx_setParameter(c, ZSTD_c_enableLongDistanceMatching, 1);
        }
    }
    ~ZstdCtx() { ZSTD_freeCCtx(c); }
};

// Join the segments' sequence lists into one for the chunk. A segment's trailing literals lead
// the next one's first sequence; where a segment's last match ran past its end, the next
// segment's parse is trimmed to start where that match ended.
// Returns the bytes the result accounts for (sequences plus the literals left at the end).
static uint64_t join_segments(const Seq* seqs, const uint32_t* counts, const uint32_t* ends, const uint32_t* trailing,
                              uint32_t nseg, uint32_t seg, uint32_t maxseq, std::vector<Seq>& all) {
    uint32_t carry = 0;    // literals waiting to lead the next sequence
    uint32_t covered = 0;  // the chunk is accounted for up to here (sequences and carry)
    for (uint32_t s = 0; s < nseg; s++) {
        uint32_t pos = s * seg;  // where the next sequence of this segment starts
        const Seq* q = seqs + (size_t)s * maxseq;
        for (uint32_t k = 0; k < counts[s]; k++) {
            Seq x = q[k];
            uint32_t end = pos + x.litLength + x.matchLength;
            if (end <= covered) {  // inside an earlier segment's long match
                pos = end;
                continue;
            }
            if (pos < covered) {  // partly inside it: keep what lies beyond
                if (pos + x.litLength >= covered) {
                    x.litLength -= covered - pos;
                } else {
                    uint32_t left = end - covered;
                    if (left < 3) {  // too short for a match: literals
                        carry += left;
                        covered = end;
                        pos = end;
                        continue;
                    }
                    x.litLength = 0;
                    x.matchLength = left;
                }
            }
            x.litLength += carry;
            carry = 0;
            all.push_back(x);
            covered = end;
            pos = end;
        }
        uint32_t e = ends[s];  // the literals after the last match, beyond what is covered
        if (e > covered) {
            carry += e - std::max(pos, covered);
            covered = e;
        }
    }
    uint64_t total = carry;
    for (const Seq& x : all) total += x.litLength + x.matchLength;
    return total;
}

int main(int argc, char** argv) {
    if (argc < 2) {
        fprintf(stderr,
                "usage: gpuzstd <file> [-o out.zst] [--chunk-mb 128] [--seg 4096] [--passes 2] [--sample 8]\n"
                "               [--orders 32,16,8,4] [--pred 4] [--pred4 4] [--key64] [--baseline]\n");
        return 2;
    }
    std::string out_path;
    size_t chunk = 128u << 20;
    int passes = 2, sample = 8, pred = 4, pred4 = 4;
    std::vector<int> orders = {32, 16, 8, 4};
    uint32_t seg = 4096;
    bool baseline = false, key32 = true;
    for (int i = 2; i < argc; i++) {
        std::string a = argv[i];
        if (a == "-o" && i + 1 < argc) out_path = argv[++i];
        else if (a == "--chunk-mb" && i + 1 < argc) chunk = (size_t)atoi(argv[++i]) << 20;
        else if (a == "--passes" && i + 1 < argc) passes = atoi(argv[++i]);
        else if (a == "--seg" && i + 1 < argc) seg = (uint32_t)atoi(argv[++i]);
        else if (a == "--sample" && i + 1 < argc) sample = atoi(argv[++i]);
        else if (a == "--orders" && i + 1 < argc) {
            orders.clear();
            for (char* tok = strtok(argv[++i], ","); tok; tok = strtok(nullptr, ",")) orders.push_back(atoi(tok));
        } else if (a == "--pred" && i + 1 < argc) pred = atoi(argv[++i]);
        else if (a == "--pred4" && i + 1 < argc) pred4 = atoi(argv[++i]);
        else if (a == "--key64") key32 = false;
        else if (a == "--baseline") baseline = true;
    }
    FILE* f = fopen(argv[1], "rb");
    if (!f) {
        fprintf(stderr, "cannot open %s\n", argv[1]);
        return 1;
    }
    fseek(f, 0, SEEK_END);
    size_t size = (size_t)_ftelli64(f);
    fseek(f, 0, SEEK_SET);
    std::vector<uint8_t> data(size + PAD, 0);
    if (fread(data.data(), 1, size, f) != size) return 1;
    fclose(f);
    chunk = std::min(chunk, size);
    int window_log = 0;
    while ((1ull << window_log) < chunk) window_log++;
    uint32_t nseg_max = (uint32_t)((chunk + seg - 1) / seg);
    uint32_t maxseq = seg / 3 + 1;

    uint8_t* d_data;
    void *d_key, *d_keys;
    uint32_t *d_pos, *d_poss, *d_counts, *d_ends, *d_trailing;
    Cand* d_cand;
    Rec* d_rec;
    Seq* d_seqs;
    Stats* d_stats;
    CK(cudaMalloc(&d_data, chunk + PAD));
    CK(cudaMalloc(&d_key, chunk * 8));
    CK(cudaMalloc(&d_keys, chunk * 8));
    CK(cudaMalloc(&d_pos, chunk * 4));
    CK(cudaMalloc(&d_poss, chunk * 4));
    CK(cudaMalloc(&d_cand, (size_t)nseg_max * seg * KC * sizeof(Cand)));
    CK(cudaMalloc(&d_rec, (size_t)nseg_max * seg * sizeof(Rec)));
    CK(cudaMalloc(&d_seqs, (size_t)nseg_max * maxseq * sizeof(Seq)));
    CK(cudaMalloc(&d_counts, nseg_max * 4));
    CK(cudaMalloc(&d_ends, nseg_max * 4));
    CK(cudaMalloc(&d_trailing, nseg_max * 4));
    CK(cudaMalloc(&d_stats, sizeof(Stats)));
    size_t tmp4 = 0, tmp8 = 0;
    cub::DeviceRadixSort::SortPairs(nullptr, tmp4, (uint32_t*)d_key, (uint32_t*)d_keys, d_pos, d_poss, (int)chunk);
    cub::DeviceRadixSort::SortPairs(nullptr, tmp8, (uint64_t*)d_key, (uint64_t*)d_keys, d_pos, d_poss, (int)chunk);
    void* d_tmp;
    CK(cudaMalloc(&d_tmp, std::max(tmp4, tmp8)));
    cudaEvent_t ev[6];
    for (auto& e : ev) CK(cudaEventCreate(&e));

    std::vector<Seq> seqs_h((size_t)nseg_max * maxseq);
    std::vector<uint32_t> counts(nseg_max), ends(nseg_max), trailing(nseg_max);
    std::vector<uint8_t> out;
    out.reserve(size / 2 + (1 << 20));
    size_t nchunks = (size + chunk - 1) / chunk;
    std::vector<std::vector<uint8_t>> frames(nchunks);
    std::vector<double> cpu_times(nchunks);
    std::vector<std::thread> workers;
    bool failed = false;
    double t_find = 0, t_parse1 = 0, t_parse2 = 0, t_cpu = 0, t_h2d = 0, t_join = 0;
    size_t total_seqs = 0;
    std::string olist;
    for (int o : orders) olist += (olist.empty() ? "" : ",") + std::to_string(o);
    printf("%s: %zu MB in chunks of %zu MB (window 2^%d); orders %s (%d-bit keys), %d predecessors (%d in the 4-byte order); "
           "segments of %u; %d pass%s, pass 1 on 1/%d of the segments\n",
           argv[1], size >> 20, chunk >> 20, window_log, olist.c_str(), key32 ? 32 : 64, pred, pred4, seg, passes,
           passes == 1 ? "" : "es", sample);
    auto t_all = clk::now();
    for (size_t at = 0; at < size; at += chunk) {
        uint32_t n = (uint32_t)std::min(chunk, size - at);
        uint32_t nseg = (n + seg - 1) / seg;
        unsigned blocks = (n + 255) / 256;
        CK(cudaEventRecord(ev[0]));
        CK(cudaMemcpy(d_data, data.data() + at, n + PAD, cudaMemcpyHostToDevice));
        CK(cudaMemset(d_cand, 0, (size_t)nseg * seg * KC * sizeof(Cand)));
        CK(cudaEventRecord(ev[1]));
        // 1. match finding, one sort order at a time
        for (int o : orders) {
            bool k64 = o != 4 && !key32;
            if (o == 4) k_keys<4, false><<<blocks, 256>>>(d_data, n, d_key, d_pos);
            else if (o == 8 && k64) k_keys<8, false><<<blocks, 256>>>(d_data, n, d_key, d_pos);
            else if (o == 8) k_keys<8, true><<<blocks, 256>>>(d_data, n, d_key, d_pos);
            else if (o == 16 && k64) k_keys<16, false><<<blocks, 256>>>(d_data, n, d_key, d_pos);
            else if (o == 16) k_keys<16, true><<<blocks, 256>>>(d_data, n, d_key, d_pos);
            else if (k64) k_keys<32, false><<<blocks, 256>>>(d_data, n, d_key, d_pos);
            else k_keys<32, true><<<blocks, 256>>>(d_data, n, d_key, d_pos);
            int minlen = o == 4 ? 3 : o, slot_near = o == 4 ? 0 : -1, slot_best = o <= 8 ? 1 : 2;
            int np = o == 4 ? pred4 : pred;
            if (k64) {
                size_t tb = tmp8;
                cub::DeviceRadixSort::SortPairs(d_tmp, tb, (uint64_t*)d_key, (uint64_t*)d_keys, d_pos, d_poss, (int)n);
                k_candidates<<<blocks, 256>>>(d_data, n, seg, nseg, (const uint64_t*)d_keys, d_poss, np, minlen, slot_near,
                                              slot_best, d_cand);
            } else {
                size_t tb = tmp4;
                cub::DeviceRadixSort::SortPairs(d_tmp, tb, (uint32_t*)d_key, (uint32_t*)d_keys, d_pos, d_poss, (int)n);
                k_candidates<<<blocks, 256>>>(d_data, n, seg, nseg, (const uint32_t*)d_keys, d_poss, np, minlen, slot_near,
                                              slot_best, d_cand);
            }
        }
        CK(cudaEventRecord(ev[2]));
        if (passes == 1) CK(cudaEventRecord(ev[3]));
        CK(cudaGetLastError());
        // 2. the parse: flat prices first (literals from a sample of the chunk), then priced
        // with the statistics of the pass before
        {
            unsigned h[256] = {};
            for (uint32_t i = 0; i < n; i += 16) h[data[at + i]]++;
            float lit[256], ll[36], ml[53], off[32];
            price_table<256>(h, 1.0f, 11.0f, lit);
            for (auto& x : ll) x = 4.0f;
            for (auto& x : ml) x = 4.0f;
            for (auto& x : off) x = 5.0f;
            set_prices(lit, ll, ml, off);
        }
        for (int pass = 1; pass <= passes; pass++) {
            bool last = pass == passes;
            uint32_t stride = last ? 1 : (uint32_t)std::max(1, sample);
            uint32_t nthreads = (nseg + stride - 1) / stride;
            if (!last) CK(cudaMemset(d_stats, 0, sizeof(Stats)));
            k_parse<<<(nthreads + PARSE_WARPS - 1) / PARSE_WARPS, PARSE_WARPS * 32>>>(
                d_data, n, seg, nseg, maxseq, stride, d_cand, d_rec, d_seqs, d_counts, d_ends, d_trailing,
                last ? nullptr : d_stats);
            CK(cudaGetLastError());
            if (!last) {
                CK(cudaEventRecord(ev[3]));
                Stats st;
                CK(cudaMemcpy(&st, d_stats, sizeof st, cudaMemcpyDeviceToHost));
                float lit[256], ll[36], ml[53], off[32];
                price_table<256>(st.lit, 1.0f, 11.0f, lit);
                price_table<36>(st.ll, 0.5f, 14.0f, ll);
                price_table<53>(st.ml, 0.5f, 14.0f, ml);
                price_table<32>(st.off, 0.5f, 14.0f, off);
                set_prices(lit, ll, ml, off);
            }
        }
        CK(cudaEventRecord(ev[4]));
        CK(cudaMemcpy(seqs_h.data(), d_seqs, (size_t)nseg * maxseq * sizeof(Seq), cudaMemcpyDeviceToHost));
        CK(cudaMemcpy(counts.data(), d_counts, nseg * 4, cudaMemcpyDeviceToHost));
        CK(cudaMemcpy(ends.data(), d_ends, nseg * 4, cudaMemcpyDeviceToHost));
        CK(cudaMemcpy(trailing.data(), d_trailing, nseg * 4, cudaMemcpyDeviceToHost));
        CK(cudaEventRecord(ev[5]));
        CK(cudaEventSynchronize(ev[5]));
        t_h2d += ms(ev[0], ev[1]);
        t_find += ms(ev[1], ev[2]);
        t_parse1 += passes == 1 ? 0 : ms(ev[2], ev[3]);
        t_parse2 += ms(ev[3], ev[4]);
        // 3. join the segments, then let libzstd entropy-code the chunk as one frame on a CPU
        // thread while the GPU takes the next chunk
        auto t0 = clk::now();
        std::vector<Seq> all;
        all.reserve(n / 8);
        uint64_t accounted = join_segments(seqs_h.data(), counts.data(), ends.data(), trailing.data(), nseg, seg, maxseq, all);
        if (accounted != n) {
            fprintf(stderr, "the joined sequences cover %llu of %u bytes\n", (unsigned long long)accounted, n);
            return 1;
        }
        total_seqs += all.size();
        t_join += since(t0);
        size_t ci = at / chunk;
        workers.emplace_back([&, ci, at, n, all = std::move(all)]() {
            auto t1 = clk::now();
            ZstdCtx z(19, window_log, true);
            std::vector<uint8_t>& fr = frames[ci];
            fr.resize(ZSTD_compressBound(n));
            size_t r = ZSTD_compressSequences(z.c, fr.data(), fr.size(), (const ZSTD_Sequence*)all.data(), all.size(),
                                              data.data() + at, n);
            if (ZSTD_isError(r)) {
                fprintf(stderr, "ZSTD_compressSequences: %s\n", ZSTD_getErrorName(r));
                fr.clear();
                failed = true;
            } else {
                fr.resize(r);
            }
            cpu_times[ci] = since(t1);
        });
    }
    for (auto& w : workers) w.join();
    double wall = since(t_all);
    if (failed) return 1;
    for (auto& fr : frames) out.insert(out.end(), fr.begin(), fr.end());
    for (double x : cpu_times) t_cpu += x;
    // verify: the output must decode to the input
    {
        std::vector<uint8_t> back(size);
        size_t at = 0, in = 0;
        while (in < out.size()) {
            size_t fsize = ZSTD_findFrameCompressedSize(out.data() + in, out.size() - in);
            if (ZSTD_isError(fsize)) {
                fprintf(stderr, "bad frame: %s\n", ZSTD_getErrorName(fsize));
                return 1;
            }
            size_t d = ZSTD_decompress(back.data() + at, size - at, out.data() + in, fsize);
            if (ZSTD_isError(d)) {
                fprintf(stderr, "decompress: %s\n", ZSTD_getErrorName(d));
                return 1;
            }
            at += d;
            in += fsize;
        }
        bool ok = at == size && memcmp(back.data(), data.data(), size) == 0;
        printf("output %zu bytes, ratio %.3f, %s; %zu sequences\n", out.size(), (double)size / out.size(),
               ok ? "decodes to the input" : "WRONG OUTPUT", total_seqs);
        if (!ok) return 1;
    }
    double gpu = t_h2d + t_find + t_parse1 + t_parse2;
    printf("GPU %.0f ms = %.2f GB/s  [upload %.0f · find %.0f · parse %.0f + %.0f ms]; CPU: join %.0f ms, entropy coding %.0f ms "
           "(%.0f MB/s per thread, overlapped with the GPU); total %.2f s = %.2f GB/s\n",
           gpu, size / gpu / 1e6, t_h2d, t_find, t_parse1, t_parse2, t_join * 1e3, t_cpu * 1e3, size / t_cpu / 1e6, wall,
           size / wall / 1e9);
    if (!out_path.empty()) {
        FILE* o = fopen(out_path.c_str(), "wb");
        if (o) {
            fwrite(out.data(), 1, out.size(), o);
            fclose(o);
        }
    }
    if (baseline) {
        std::vector<uint8_t> b(ZSTD_compressBound(size));
        auto t0 = clk::now();
        size_t total = 0;
        for (size_t at = 0; at < size; at += chunk) {
            size_t n = std::min(chunk, size - at);
            ZstdCtx z(19, window_log, false);
            size_t r = ZSTD_compress2(z.c, b.data() + total, b.size() - total, data.data() + at, n);
            if (ZSTD_isError(r)) return 1;
            total += r;
        }
        double s = since(t0);
        printf("zstd -19 + LDM, same window, one thread: %zu bytes, ratio %.3f, %.1f s (%.1f MB/s)\n", total, (double)size / total,
               s, size / s / 1e6);
    }
    return 0;
}
