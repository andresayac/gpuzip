// inflate.cuh - a compact Deflate (RFC 1951) decoder that compiles for both the CPU and the
// GPU, built for speculative parallel decoding:
//   * it does not copy matches: every output position gets either its byte or a back-reference
//     ("the byte d positions earlier"), so decoding never waits on memory reads; references
//     are resolved afterwards, all at once, by pointer jumping. A piece that starts in the
//     middle of the stream needs nothing special: its references into the 32 KB before it
//     simply point into the previous piece;
//   * it stops when it lands exactly on one of a sorted list of candidate block starts;
//   * plausible_dynamic_header() says whether a bit position looks like the start of a
//     dynamic-Huffman block, applying the same checks zlib applies to every real block, and
//     header_prefilter*() reject most other positions from 64 or 128 bits alone.
//
// Input buffers must be 4-byte aligned and zero-padded to a multiple of 4 bytes plus 32.
#pragma once
#include <stdint.h>
#include <string.h>

#if defined(__CUDACC__)
#define GI_HD __host__ __device__
#define GI_INL __host__ __device__ __forceinline__
#define GI_NOINL __host__ __device__ __noinline__  // big, once-per-block work: keeps kernels small to compile
#else
#define GI_HD
#define GI_INL inline
#define GI_NOINL inline
#endif

namespace gi {

constexpr int WINDOW = 32768;
constexpr uint16_t REF = 255;  // output values > REF are back-references: the byte (value - REF) positions back
#ifndef GI_LIT_LB
#define GI_LIT_LB 10
#endif
constexpr int LIT_LB = GI_LIT_LB;  // first-level lookup bits of the literal/length code
constexpr int DIST_LB = 8;
constexpr int LUT_U16 = (1 << LIT_LB) + (1 << DIST_LB);  // lookup storage per decoder

// ERR_OVERFLOW: the buffer filled (or the block budget ran out); STOPPED: reached the stop bit.
// Both leave Result::resume, the exact state to continue from.
enum Status : int32_t { OK = 0, ERR_DATA = 1, ERR_OVERFLOW = 2, ERR_INPUT = 3, STOPPED = 4 };

// Where a decoder starts, or a target it may land on: at a block header; between two symbols
// of a fixed-Huffman block; or between two symbols of a dynamic block whose header is at
// `header` (its tables are rebuilt from there)
enum Mode : uint8_t { AT_BLOCK = 0, IN_FIXED = 1, IN_DYNAMIC = 2 };

struct Start {
    uint64_t bit;
    uint64_t header;  // IN_DYNAMIC: the block's header; IN_FIXED: the header if known, else UINT64_MAX
    uint8_t mode;
    int8_t final;     // BFINAL of the block it starts inside: 0, 1, or -1 when unknown
};

struct Result {
    uint64_t end_bit;   // where decoding stopped
    uint64_t eob_bit;   // just after the first end-of-block code (UINT64_MAX: none)
    Start resume;       // ERR_OVERFLOW: where to continue, with a new buffer
    uint32_t eob_out;   // symbols written by then
    uint32_t out_len;   // symbols written
    int32_t status;
    int32_t landed;     // index in the target list it stopped on; -1: end of the stream
    int32_t end_final;  // BFINAL of the block it stopped in; -1: unknown
    uint32_t blocks;
    uint64_t last_block_bit;  // header of the last block this run started (UINT64_MAX: none)
    uint32_t last_block_out;  // symbols written before it: after an error, where the damage may begin
    uint32_t symbols;         // literals and matches decoded (zlib's blocks hold a fixed number)
};

constexpr uint32_t MAX_SYMBOL_OUT = 258;  // the most one symbol writes
constexpr uint32_t MAX_BLOCKS = 1u << 16;  // blocks one run() decodes at most: bounds a kernel's time

// RFC 1951 tables, in constant memory on the GPU and plain arrays on the CPU
#define GI_LBASE {3,4,5,6,7,8,9,10,11,13,15,17,19,23,27,31,35,43,51,59,67,83,99,115,131,163,195,227,258}
#define GI_LEXT {0,0,0,0,0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,0}
#define GI_DBASE {1,2,3,4,5,7,9,13,17,25,33,49,65,97,129,193,257,385,513,769,1025,1537,2049,3073,4097,6145,8193,12289,16385,24577}
#define GI_DEXT {0,0,0,0,1,1,2,2,3,3,4,4,5,5,6,6,7,7,8,8,9,9,10,10,11,11,12,12,13,13}
#define GI_ORDER {16,17,18,0,8,7,9,6,10,5,11,4,12,3,13,2,14,1,15}
// On the GPU the tables sit in global memory and are read through the read-only cache, which
// serves 32 different addresses at once; constant memory would serialize them.
#if defined(__CUDACC__)
__device__ const uint16_t c_lbase[29] = GI_LBASE;
__device__ const uint8_t c_lext[29] = GI_LEXT;
__device__ const uint16_t c_dbase[30] = GI_DBASE;
__device__ const uint8_t c_dext[30] = GI_DEXT;
__device__ const uint8_t c_order[19] = GI_ORDER;
#endif
static const uint16_t h_lbase[29] = GI_LBASE;
static const uint8_t h_lext[29] = GI_LEXT;
static const uint16_t h_dbase[30] = GI_DBASE;
static const uint8_t h_dext[30] = GI_DEXT;
static const uint8_t h_order[19] = GI_ORDER;
#if defined(__CUDA_ARCH__)
#define GI_TAB(name, i) __ldg(&c_##name[i])
#else
#define GI_TAB(name, i) h_##name[i]
#endif

GI_INL uint32_t load_word(const uint32_t* p) {
#if defined(__CUDA_ARCH__)
    return __ldg(p);
#else
    return *p;
#endif
}

// LSB-first bit reader that loads aligned 32-bit words
struct Bits {
    const uint32_t* w;
    uint64_t nwords;
    uint64_t size_bits;
    uint64_t next;  // next word to load
    uint64_t buf;
    int cnt;        // valid bits in buf

    GI_INL void refill() {
        while (cnt <= 32) {
            uint64_t v = next < nwords ? load_word(w + next) : 0;
            buf |= v << cnt;
            next++;
            cnt += 32;
        }
    }
    GI_INL void init(const uint8_t* d, uint64_t n, uint64_t bit) {
        w = (const uint32_t*)d;
        nwords = (n + 3) / 4;
        size_bits = n * 8;
        next = bit >> 5;
        buf = 0;
        cnt = 0;
        refill();
        int skip = (int)(bit & 31);
        buf >>= skip;
        cnt -= skip;
    }
    GI_INL uint32_t peek(int n) const { return (uint32_t)(buf & ((1ull << n) - 1)); }
    GI_INL void drop(int n) { buf >>= n; cnt -= n; }
    GI_INL uint32_t get(int n) {
        if (cnt < n) refill();
        uint32_t v = peek(n);
        drop(n);
        return v;
    }
    GI_INL uint64_t pos() const { return next * 32 - cnt; }
    GI_INL bool overrun() const { return pos() > size_bits; }
    GI_INL void align_byte() {
        int r = (int)(pos() & 7);
        if (r) get(8 - r);
    }
};

// Canonical Huffman code: a first-level lookup table (lut, storage provided by the caller)
// for codes up to LB bits, and the count/symbol arrays of zlib's puff.c for longer codes
template <int LB, int NSYM>
struct Huff {
    uint16_t* lut;  // (length << 9) | symbol; 0 = longer code (or none)
    uint16_t count[16];
    uint16_t symbol[NSYM];
    int nonzero;

    // 0: complete code; 1: incomplete; -1: over-subscribed
    GI_NOINL int build(const uint8_t* lens, int n, bool with_lut = true) {
        for (int i = 0; i < 16; i++) count[i] = 0;
        for (int s = 0; s < n; s++) count[lens[s]]++;
        nonzero = n - count[0];
        count[0] = 0;
        int left = 1;
        for (int len = 1; len < 16; len++) {
            left <<= 1;
            left -= count[len];
            if (left < 0) return -1;
        }
        uint16_t offs[16];
        offs[1] = 0;
        for (int len = 1; len < 15; len++) offs[len + 1] = offs[len] + count[len];
        uint16_t next[16];
        int code = 0;
        for (int bits = 1; bits < 16; bits++) {
            code = (code + count[bits - 1]) << 1;
            next[bits] = (uint16_t)code;
        }
        if (with_lut)
            for (int i = 0; i < (1 << LB); i++) lut[i] = 0;
        for (int s = 0; s < n; s++) {
            int len = lens[s];
            if (!len) continue;
            symbol[offs[len]++] = (uint16_t)s;
            int c = next[len]++;
            if (with_lut && len <= LB) {
                int rev = 0;
                for (int b = 0; b < len; b++) rev |= ((c >> b) & 1) << (len - 1 - b);
                for (int k = rev; k < (1 << LB); k += 1 << len) lut[k] = (uint16_t)((len << 9) | s);
            }
        }
        return left > 0 ? 1 : 0;
    }

    // zlib accepts an incomplete code only when it is a single code of length 1
    GI_INL bool single_code() const { return nonzero == 1 && count[1] == 1; }

    // symbol, or -1 for a bit pattern that is not a code
    GI_INL int decode(Bits& br, bool use_lut = true) {
        if (br.cnt < 15) br.refill();
        if (use_lut) {
            uint16_t e = lut[br.peek(LB)];
            if (e) {
                br.drop(e >> 9);
                return e & 511;
            }
        }
        uint64_t b = br.buf;
        int code = 0, first = 0, index = 0;
        for (int len = 1; len < 16; len++) {
            code |= (int)(b & 1);
            b >>= 1;
            int c = count[len];
            if (code - c < first) {
                br.drop(len);
                return symbol[index + (code - first)];
            }
            index += c;
            first += c;
            first <<= 1;
            code <<= 1;
        }
        return -1;
    }
};

// Reads a dynamic block's code lengths into lens[] (after BFINAL and BTYPE). 0 = valid.
template <class CL>
GI_NOINL int read_code_lengths(Bits& br, CL& clh, uint8_t* lens, int& hlit, int& hdist, bool with_lut) {
    hlit = (int)br.get(5) + 257;
    hdist = (int)br.get(5) + 1;
    int hclen = (int)br.get(4) + 4;
    if (hlit > 286 || hdist > 30) return -1;
    uint8_t cl[19];
    for (int i = 0; i < 19; i++) cl[i] = 0;
    for (int i = 0; i < hclen; i++) cl[GI_TAB(order, i)] = (uint8_t)br.get(3);
    if (clh.build(cl, 19, with_lut) != 0) return -1;  // zlib: the code-length code must be complete
    int total = hlit + hdist;
    for (int i = 0; i < total;) {
        int s = clh.decode(br, with_lut);
        if (s < 0) return -1;
        if (s < 16) {
            lens[i++] = (uint8_t)s;
            continue;
        }
        int rep;
        uint8_t val = 0;
        if (s == 16) {
            if (i == 0) return -1;
            val = lens[i - 1];
            rep = 3 + (int)br.get(2);
        } else if (s == 17) {
            rep = 3 + (int)br.get(3);
        } else {
            rep = 11 + (int)br.get(7);
        }
        if (i + rep > total) return -1;
        while (rep--) lens[i++] = val;
    }
    if (lens[256] == 0) return -1;  // no end-of-block code
    return 0;
}

// Kraft check only, for the block finder: 0 complete, 1 incomplete, -1 over-subscribed
GI_INL int kraft(const uint8_t* lens, int n, int& nonzero, int& ones) {
    int count[16];
    for (int i = 0; i < 16; i++) count[i] = 0;
    for (int s = 0; s < n; s++) count[lens[s]]++;
    nonzero = n - count[0];
    ones = count[1];
    int left = 1;
    for (int len = 1; len < 16; len++) {
        left <<= 1;
        left -= count[len];
        if (left < 0) return -1;
    }
    return left > 0 ? 1 : 0;
}

// Cheap first test of a candidate block start, from the 64 bits that begin at it: a dynamic
// block, HLIT and HDIST in range, and a code-length code that can still be complete.
GI_INL bool header_prefilter(uint64_t v) {
    if (((v >> 1) & 3) != 2 || ((v >> 3) & 31) > 29 || ((v >> 8) & 31) > 29) return false;
    int hclen = (int)((v >> 13) & 15) + 4;
    int n = hclen < 15 ? hclen : 15;
    int sum = 0;
    for (int i = 0; i < n; i++) {
        int l = (int)((v >> (17 + 3 * i)) & 7);
        sum += l ? 128 >> l : 0;
    }
    return sum <= 128 && (hclen > 15 || sum == 128);
}

// Stronger first test from the 128 bits that begin at a candidate (v1: bits 0-63, v2: 64-127):
// the code-length code must be complete, as zlib requires. Same work for every candidate, so
// a warp of candidates runs it without diverging.
GI_INL bool header_prefilter128(uint64_t v1, uint64_t v2) {
    if (((v1 >> 1) & 3) != 2 || ((v1 >> 3) & 31) > 29 || ((v1 >> 8) & 31) > 29) return false;
    int hclen = (int)((v1 >> 13) & 15) + 4;
    int sum = 0;
    for (int i = 0; i < 19; i++) {
        int p = 17 + 3 * i;
        uint32_t l = p + 3 <= 64 ? (uint32_t)(v1 >> p) : p >= 64 ? (uint32_t)(v2 >> (p - 64)) : (uint32_t)((v1 >> p) | (v2 << (64 - p)));
        l &= 7;
        sum += i < hclen && l ? 128 >> l : 0;
    }
    return sum == 128;
}

// Does `bit` look like the start of a dynamic-Huffman block? Applies every check zlib applies
// to a block header, so real block starts always pass and random positions very rarely do.
GI_INL bool plausible_dynamic_header(const uint8_t* in, uint64_t size, uint64_t bit) {
    Bits br;
    br.init(in, size, bit);
    uint32_t h = br.peek(17);  // BFINAL 1, BTYPE 2, HLIT 5, HDIST 5, HCLEN 4
    if (((h >> 1) & 3) != 2 || ((h >> 3) & 31) > 29 || ((h >> 8) & 31) > 29) return false;
    br.drop(3);
    Huff<7, 19> clh;
    clh.lut = nullptr;
    uint8_t lens[320];
    int hlit, hdist;
    if (read_code_lengths(br, clh, lens, hlit, hdist, false) != 0) return false;
    int nz, ones;
    int r = kraft(lens, hlit, nz, ones);
    if (r < 0 || (r > 0 && !(nz == 1 && ones == 1))) return false;
    r = kraft(lens + hlit, hdist, nz, ones);
    if (r < 0 || (r > 0 && !(nz == 0 || (nz == 1 && ones == 1)))) return false;
    return !br.overrun();
}

// p[0..n) = v, 16 bytes at a time where the alignment allows: a long match is a few stores
GI_INL void fill16(uint16_t* p, uint32_t n, uint16_t v) {
    while (n && ((uintptr_t)p & 15)) {
        *p++ = v;
        n--;
    }
    uint32_t w = (uint32_t)v | ((uint32_t)v << 16);
#if defined(__CUDA_ARCH__)
    uint4 q = make_uint4(w, w, w, w);
    for (; n >= 8; n -= 8, p += 8) *(uint4*)p = q;
#else
    uint64_t q = (uint64_t)w | ((uint64_t)w << 32);
    for (; n >= 4; n -= 4, p += 4) memcpy(p, &q, 8);
#endif
    while (n--) *p++ = v;
}

// Stored blocks are raw bytes of known length: a GPU decoder notes where they go and skips
// them in no time, and a separate kernel copies them all at once.
struct StoredCopy {
    const uint8_t* src;
    uint16_t* dst;
    uint32_t len;
};

struct StoredCopies {
    StoredCopy* list;
    unsigned* count;
    unsigned cap;

    // false when the list is full (or on the CPU): the caller copies the bytes itself
    GI_INL bool add(const uint8_t* src, uint16_t* dst, uint32_t len) {
#if defined(__CUDA_ARCH__)
        unsigned i = atomicAdd(count, 1u);
        if (i >= cap) return false;
        list[i] = {src, dst, len};
        return true;
#else
        return false;
#endif
    }
};

struct Decoder {
    Huff<LIT_LB, 288> lit;
    Huff<DIST_LB, 30> dist;
    Huff<7, 19> clh;
    uint16_t clh_lut[128];
    uint8_t lens[320];
    bool have_fixed;  // the tables hold the fixed code: a run of fixed blocks builds them once

    // the literal and distance lookup tables live in caller memory (shared memory on the GPU)
    GI_INL void attach(uint16_t* luts) {
        lit.lut = luts;
        dist.lut = luts + (1 << LIT_LB);
        clh.lut = clh_lut;
        have_fixed = false;
    }

    GI_NOINL int dynamic_tables(Bits& br) {
        have_fixed = false;
        int hlit, hdist;
        if (read_code_lengths(br, clh, lens, hlit, hdist, true) != 0) return -1;
        int r = lit.build(lens, hlit);
        if (r < 0 || (r > 0 && !lit.single_code())) return -1;
        r = dist.build(lens + hlit, hdist);
        if (r < 0 || (r > 0 && !(dist.nonzero == 0 || dist.single_code()))) return -1;
        return 0;
    }

    GI_NOINL void fixed_tables() {
        if (have_fixed) return;
        have_fixed = true;
        for (int i = 0; i < 288; i++) lens[i] = i < 144 ? 8 : i < 256 ? 9 : i < 280 ? 7 : 8;
        lit.build(lens, 288);
        for (int i = 0; i < 30; i++) lens[i] = 5;
        dist.build(lens, 30);
    }

    // The symbols of one block, up to its end-of-block code: 0. With LAND, also checks before
    // every symbol for a target between symbols of this block: 1, r.landed set. A full buffer
    // or the stop bit: -1, ERR_OVERFLOW or STOPPED with r.resume at the symbol it did not decode.
    // Bad data: -1, ERR_DATA; past the end of the input: -1, ERR_INPUT. limit is
    // min(stop, input bits + 1), so one comparison per symbol covers both.
    template <bool LAND>
    GI_INL int symbols(Bits& br, uint16_t* out, uint32_t& op, uint32_t cap, bool stream_start, const Start* targets,
                       int ntargets, int& t, bool fixed, uint64_t header, int final, uint64_t limit, Result& r) {
        uint8_t mode = fixed ? IN_FIXED : IN_DYNAMIC;
        for (;;) {
            if (LAND) {
                uint64_t p = br.pos();
                while (t < ntargets && targets[t].bit < p) t++;
                if (t < ntargets && targets[t].bit == p && targets[t].mode == mode &&
                    (fixed || targets[t].header == header)) {
                    r.landed = t;
                    return 1;
                }
            }
            if (op + MAX_SYMBOL_OUT > cap || br.pos() >= limit) {
                uint64_t p = br.pos();
                if (p > br.size_bits) { r.status = ERR_INPUT; return -1; }
                r.status = p >= limit ? STOPPED : ERR_OVERFLOW;
                r.resume.bit = p;
                r.resume.header = header;
                r.resume.mode = mode;
                r.resume.final = (int8_t)final;
                return -1;
            }
            int sym = lit.decode(br);
            if (sym < 256) {
                if (sym < 0) { r.status = ERR_DATA; return -1; }
                out[op++] = (uint16_t)sym;
                r.symbols++;
                continue;
            }
            if (sym == 256) return 0;
            r.symbols++;
            sym -= 257;
            if (sym >= 29) { r.status = ERR_DATA; return -1; }
            uint32_t len = GI_TAB(lbase, sym) + br.get(GI_TAB(lext, sym));
            int ds = dist.decode(br);
            if (ds < 0 || ds >= 30) { r.status = ERR_DATA; return -1; }
            uint32_t d = GI_TAB(dbase, ds) + br.get(GI_TAB(dext, ds));
            if (d > op && (stream_start || d - op > (uint32_t)WINDOW)) { r.status = ERR_DATA; return -1; }
            uint16_t ref = (uint16_t)(REF + d);  // each byte of the match is the byte d back
            if (len >= 16) {
                fill16(out + op, len, ref);
            } else {
                for (uint32_t k = 0; k < len; k++) out[op + k] = ref;
            }
            op += len;
        }
    }

    // Decode from s until landing on one of targets[0..ntargets) (sorted by bit) or the end of
    // the stream, writing bytes and back-references to out. A start inside a block whose BFINAL
    // is unknown (s.final -1) goes on past that block's end; the caller learns from the piece
    // before whether it was the last block, and if so keeps only the output up to eob_out.
    // stream_start: nothing comes before, so a reference before the output is an error.
    // stop: at the first block or symbol boundary at or after it, stop with STOPPED.
    GI_HD Result run(const uint8_t* in, uint64_t size, Start s, const Start* targets, int ntargets, uint16_t* out,
                     uint32_t cap, bool stream_start, uint64_t stop = UINT64_MAX, StoredCopies* stored = nullptr) {
        Result r;
        r.eob_bit = UINT64_MAX;
        r.eob_out = 0;
        r.out_len = 0;
        r.status = OK;
        r.landed = -1;
        r.end_final = s.mode == AT_BLOCK ? -1 : s.final;
        r.blocks = 0;
        r.last_block_bit = UINT64_MAX;
        r.last_block_out = 0;
        r.symbols = 0;
        Bits br;
        uint32_t op = 0;
        int t = 0;
        bool in_block = false, fixed = false;
        uint64_t header = s.header;
        if (s.mode == IN_FIXED) {
            fixed_tables();
            in_block = fixed = true;
        } else if (s.mode == IN_DYNAMIC) {
            br.init(in, size, s.header);
            br.get(3);
            if (dynamic_tables(br) != 0) {
                r.status = ERR_DATA;
                r.end_bit = s.bit;
                return r;
            }
            in_block = true;
        }
        br.init(in, size, s.bit);
        uint64_t limit = stop < br.size_bits + 1 ? stop : br.size_bits + 1;
        for (;;) {
            if (!in_block) {
                uint64_t p = br.pos();
                while (t < ntargets && targets[t].bit < p) t++;
                if (t < ntargets && targets[t].bit == p && targets[t].mode == AT_BLOCK) {
                    r.landed = t;
                    break;
                }
                if (p >= stop || op + MAX_SYMBOL_OUT > cap || r.blocks >= MAX_BLOCKS) {
                    // stop here; a new decoder reads this header again
                    r.status = p >= stop ? STOPPED : ERR_OVERFLOW;
                    r.resume = {p, p, AT_BLOCK, -1};
                    break;
                }
                if (p >= br.size_bits) {
                    r.status = ERR_INPUT;
                    break;
                }
                header = p;
                r.last_block_bit = p;
                r.last_block_out = op;
                int final = (int)br.get(1);
                int type = (int)br.get(2);
                r.blocks++;
                r.end_final = final;
                if (type == 0) {
                    br.align_byte();
                    uint32_t len = br.get(16), nlen = br.get(16);
                    if (len != (~nlen & 0xffff)) { r.status = ERR_DATA; break; }
                    if (op + len > cap) {
                        r.status = ERR_OVERFLOW;
                        r.resume = {p, p, AT_BLOCK, -1};
                        break;
                    }
                    uint64_t byte = br.pos() >> 3;
                    if (byte + len > size) { r.status = ERR_INPUT; break; }
                    if (!stored || !stored->add(in + byte, out + op, len))  // else copied later, in parallel
                        for (uint32_t k = 0; k < len; k++) out[op + k] = in[byte + k];
                    op += len;
                    br.init(in, size, (byte + len) * 8);
                    if (final) break;
                    continue;
                }
                if (type == 3) { r.status = ERR_DATA; break; }
                if (type == 1) {
                    fixed_tables();
                    fixed = true;
                } else {
                    if (dynamic_tables(br) != 0) { r.status = ERR_DATA; break; }
                    fixed = false;
                }
                in_block = true;
            }
            // landing between symbols is checked only in blocks that have such targets: every
            // fixed block may, a dynamic block only when a target names its header
            bool land = fixed;
            for (int u = t; !land && u < ntargets && targets[u].mode == IN_DYNAMIC && targets[u].header == header; u++)
                land = true;
            int how = land ? symbols<true>(br, out, op, cap, stream_start, targets, ntargets, t, fixed, header,
                                           r.end_final, limit, r)
                           : symbols<false>(br, out, op, cap, stream_start, targets, ntargets, t, fixed, header,
                                            r.end_final, limit, r);
            if (how != 0) break;
            in_block = false;
            if (br.overrun()) { r.status = ERR_INPUT; break; }
            if (r.eob_bit == UINT64_MAX) {
                r.eob_bit = br.pos();
                r.eob_out = op;
            }
            if (r.end_final == 1) break;  // the last block (unknown, -1, when it started inside one)
        }
        r.end_bit = br.pos();
        r.out_len = op;
        return r;
    }

    // Huffman codes resynchronize: decoding from an arbitrary bit soon falls onto the real
    // symbol boundaries. With the tables of a block (mode AT_BLOCK: the block whose header is at
    // `header`; IN_FIXED: the fixed code; IN_DYNAMIC: the dynamic header at `header`), decodes
    // (writing nothing) from from_bit, restarting one bit later whenever the data makes no
    // sense; past an end-of-block code it reads the next header as usual. Returns where a
    // decoder can start at the first symbol boundary at or after at_bit (bit UINT64_MAX: none),
    // with BFINAL unknown. Wrong guesses are harmless: no decoder lands on a start that is not
    // a real boundary, and a decoder that lands brings the true BFINAL with it.
    GI_HD Start probe(const uint8_t* in, uint64_t size, uint8_t mode, uint64_t header, uint64_t from_bit,
                      uint64_t at_bit) {
        Start none = {UINT64_MAX, UINT64_MAX, AT_BLOCK, -1};
        bool fixed0 = mode == IN_FIXED;
        if (mode == AT_BLOCK) {
            Bits hb;
            hb.init(in, size, header);
            hb.get(1);
            int type0 = (int)hb.get(2);
            if (type0 == 0 || type0 == 3) return none;
            fixed0 = type0 == 1;
        }
        uint64_t start = from_bit;
        for (int attempt = 0; attempt < 64 && start < at_bit; attempt++) {
            bool fixed = fixed0;
            if (fixed) {
                fixed_tables();
            } else {
                Bits h2;
                h2.init(in, size, header + 3);
                if (dynamic_tables(h2) != 0) return none;
            }
            uint64_t cur = header;
            Bits br;
            br.init(in, size, start);
            uint64_t p;
            for (;;) {
                p = br.pos();
                if (p >= at_bit) return {p, cur, (uint8_t)(fixed ? IN_FIXED : IN_DYNAMIC), -1};
                int sym = lit.decode(br);
                if (sym < 0 || sym > 285) break;
                if (sym < 256) continue;
                if (sym == 256) {
                    uint64_t h = br.pos();
                    if (h >= at_bit) return {h, h, AT_BLOCK, -1};
                    br.get(1);
                    int ty = (int)br.get(2);
                    if (ty == 1) {
                        fixed_tables();
                        fixed = true;
                    } else if (ty == 2 && dynamic_tables(br) == 0) {
                        fixed = false;
                    } else {
                        break;  // a stored block or bad data: try again a bit later
                    }
                    cur = h;
                    continue;
                }
                sym -= 257;
                br.get(GI_TAB(lext, sym));
                int ds = dist.decode(br);
                if (ds < 0 || ds >= 30) break;
                br.get(GI_TAB(dext, ds));
            }
            start = p + 1;
        }
        return none;
    }
};

}  // namespace gi
