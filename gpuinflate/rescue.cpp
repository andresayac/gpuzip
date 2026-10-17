// rescue.cpp - gzrescue: recover what a damaged gzip file still holds. See rescue.h, and
// research/recovery/README.md for the method and what it recovers.
//
// 1. Every dynamic-block header in the file is found (the cheap test on every bit position and
//    zlib's header checks from the finder, each candidate confirmed by decoding two blocks).
// 2. Decoding goes block by block, landing: a decode from one header that lands exactly on the
//    next is in step; one that hits invalid data, runs off the end, reads a "final" block where
//    data goes on, or passes over the next header went through damage. Headers of Deflate
//    streams embedded in the data (a .gz inside a tar) are told apart: their chain of blocks
//    ends on its own instead of joining the stream's.
// 3. zlib fills every block with the same number of symbols (16,383 by default); a block in step
//    with another count was damaged in a way landing cannot see: a second pass flags it.
// 4. A damaged block is kept but flagged; decoding restarts at the next header; bytes that copy
//    from a damaged block, and whatever copies from those, are unknown.
// 5. The damaged block's true length comes from the trailer when there is one such block.
// 6. The unknown bytes are inferred: each is a copy of one byte of the window that was lost, and
//    every copy of it appears somewhere in the output with bytes around it. A trigram model of
//    the file's exact bytes scores each possible value over all those places; the values it is
//    sure of are kept, marked inferred, and serve as context for the next round.
// 7. Transfers in ASCII mode are undone: LF to CR LF exactly; CR LF to LF by putting a CR back
//    before the LFs where that makes a damaged block decode in step again.
#ifndef GR_BUILD
#define GR_STATIC
#endif
#include "rescue.h"
#include "inflate.cuh"

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cmath>
#include <map>
#include <set>
#include <string>
#include <unordered_map>
#include <vector>

namespace {

constexpr uint64_t SCRATCH = 32u << 20;      // symbols decoded per run() call
constexpr uint64_t VERIFY_OUT = 256u << 10;  // a header is confirmed by decoding this much (or to the end) from it
constexpr uint32_t VERIFY_BLOCKS = 2;
constexpr uint64_t FIXED_SEARCH = 8ull << 23;  // bits: how far past an error to try resynchronizing in a fixed block

struct Piece {
    uint64_t in_bit_start = 0, in_bit_end = 0;
    std::vector<uint8_t> bytes, known;  // known: 0 unknown, 1 exact, 2 inferred
    std::vector<uint32_t> var;          // an unknown byte: the window byte it copies (0: none)
    bool suspect = false;       // damaged somewhere inside
    std::string why;            // how the damage showed
    uint64_t true_len = 0;      // suspect: what its length must have been (0: unknown)
    bool stream_start = false;  // began at a gzip member's first block
    bool landed = false;        // ended by landing on the next header
    int status = gi::OK;
    uint32_t blocks = 0, symbols = 0;
    uint64_t last_block_bit = UINT64_MAX;  // header of the last block it started
    size_t last_block_at = 0;              // where that block's output begins
};

struct Member {
    size_t first_piece = 0, last_piece = 0;
    bool trailer = false;  // the size (and perhaps the CRC) of the original is known
    bool has_crc = false, size_known = false;
    uint32_t crc = 0, isize = 0;
    int crc_check = -1;  // 1 ok, 0 mismatch, -1 not checkable
};

struct Outcome {
    std::vector<Piece> pieces;
    std::vector<Member> members;
    std::vector<std::string> notes;
    bool truncated = false;
    uint64_t input_end = 0;  // byte after the first member's last block
};

uint64_t produced_so_far(const Outcome& o) {
    uint64_t n = 0;
    for (const Piece& p : o.pieces) n += p.bytes.size();
    return n;
}

uint64_t bits64(const uint8_t* in, uint64_t bit) {
    uint64_t lo, hi;
    memcpy(&lo, in + (bit >> 3), 8);
    memcpy(&hi, in + (bit >> 3) + 8, 8);
    int off = (int)(bit & 7);
    return off ? (lo >> off) | (hi << (64 - off)) : lo;
}

uint32_t crc32(const uint8_t* p, size_t n, uint32_t crc = 0) {
    static uint32_t table[256];
    if (!table[1])
        for (uint32_t i = 0; i < 256; i++) {
            uint32_t c = i;
            for (int k = 0; k < 8; k++) c = c & 1 ? 0xEDB88320u ^ (c >> 1) : c >> 1;
            table[i] = c;
        }
    crc = ~crc;
    for (size_t i = 0; i < n; i++) crc = table[(crc ^ p[i]) & 0xff] ^ (crc >> 8);
    return ~crc;
}

// gzip member header at byte h: its length, or 0 when it is not one
uint64_t gzip_header(const uint8_t* in, uint64_t size, uint64_t h) {
    if (h + 10 > size || in[h] != 0x1f || in[h + 1] != 0x8b || in[h + 2] != 8) return 0;
    uint8_t flg = in[h + 3];
    uint64_t p = h + 10;
    if (flg & 4) {
        if (p + 2 > size) return 0;
        p += 2 + (in[p] | in[p + 1] << 8);
    }
    for (uint8_t f : {8, 16}) {
        if (flg & f) {
            while (p < size && in[p]) p++;
            p++;
        }
    }
    if (flg & 2) p += 2;
    return p <= size ? p - h : 0;
}

struct Rescuer {
    const uint8_t* in;
    uint64_t size, nbits;
    gi::Decoder dec;
    uint16_t luts[gi::LUT_U16];
    std::vector<uint16_t> scratch;
    // the last 32 KB of trusted output, for references that reach before a piece's start
    std::vector<uint8_t> win_bytes, win_known;
    std::vector<uint32_t> win_var;
    bool win_valid = false;
    uint32_t next_var = 1;  // variables: the bytes of lost windows, WINDOW + 1 per window

    Rescuer(const uint8_t* d, uint64_t n) : in(d), size(n), nbits(n * 8), scratch(SCRATCH + 2 * gi::MAX_SYMBOL_OUT) { dec.attach(luts); }

    // Does decoding from s go on cleanly for long enough to trust it?
    bool verify(const gi::Start& s) {
        gi::Result r = dec.run(in, size, s, nullptr, 0, scratch.data(), (uint32_t)std::min<uint64_t>(scratch.size(), VERIFY_OUT), false);
        if (r.status == gi::OK) return true;
        if (r.status != gi::ERR_OVERFLOW) return false;
        return r.blocks >= VERIFY_BLOCKS || r.out_len >= VERIFY_OUT - gi::MAX_SYMBOL_OUT;
    }

    // Every confirmed dynamic-block header in [from, nbits)
    std::vector<gi::Start> find_headers(uint64_t from) {
        std::vector<gi::Start> h;
        for (uint64_t b = from; b + 128 < nbits; b++) {
            uint64_t v1 = bits64(in, b);
            if (!gi::header_prefilter(v1) || !gi::header_prefilter128(v1, bits64(in, b + 64))) continue;
            if (!gi::plausible_dynamic_header(in, size, b)) continue;
            gi::Start s = {b, b, gi::AT_BLOCK, -1};
            if (verify(s)) h.push_back(s);
        }
        return h;
    }

    // Inside a fixed-Huffman block the codes resynchronize: a symbol boundary after e that
    // decodes cleanly from there on. bit UINT64_MAX: none.
    gi::Start fixed_restart(uint64_t e, uint64_t limit) {
        limit = std::min(limit, std::min(nbits, e + FIXED_SEARCH));
        for (uint64_t from = e + 1; from + 4096 * 8 < limit; from += 2048 * 8) {
            gi::Start p = dec.probe(in, size, gi::IN_FIXED, UINT64_MAX, from, from + 2048 * 8);
            if (p.bit != UINT64_MAX && p.bit < limit && verify(p)) return p;
        }
        return {UINT64_MAX, UINT64_MAX, gi::AT_BLOCK, -1};
    }

    // Decode from s until it lands on one of the targets, ends the member, or fails; resolves
    // bytes and their certainty as it goes. References before the piece reach into the window
    // of trusted output when there is one, else they are unknown.
    gi::Result decode_piece(gi::Start s, bool stream_start, const gi::Start* targets, int ntargets, Piece& piece) {
        piece.in_bit_start = s.bit;
        piece.stream_start = stream_start;
        // a piece with no window before it: its references before its start are copies of
        // the lost window's bytes, each a variable
        uint32_t var_base = 0;
        if (!win_valid && !stream_start && next_var < 0xF0000000u) {
            var_base = next_var;
            next_var += gi::WINDOW + 1;
        }
        gi::Result r;
        for (;;) {
            r = dec.run(in, size, s, targets, ntargets, scratch.data(), (uint32_t)scratch.size(), stream_start && piece.bytes.empty());
            size_t base = piece.bytes.size();
            piece.bytes.resize(base + r.out_len);
            piece.known.resize(base + r.out_len);
            piece.var.resize(base + r.out_len);
            for (uint32_t i = 0; i < r.out_len; i++) {
                uint16_t v = scratch[i];
                size_t p = base + i;
                if (v <= gi::REF) {
                    piece.bytes[p] = (uint8_t)v;
                    piece.known[p] = 1;
                    piece.var[p] = 0;
                    continue;
                }
                uint32_t d = v - gi::REF;
                if (d <= p) {
                    piece.bytes[p] = piece.bytes[p - d];
                    piece.known[p] = piece.known[p - d];
                    piece.var[p] = piece.var[p - d];
                } else if (win_valid && d - p <= win_bytes.size()) {
                    size_t q = win_bytes.size() - (d - p);
                    piece.bytes[p] = win_bytes[q];
                    piece.known[p] = win_known[q];
                    piece.var[p] = win_var[q];
                } else {
                    piece.bytes[p] = 0;
                    piece.known[p] = 0;
                    piece.var[p] = var_base && d - p <= gi::WINDOW ? var_base + (uint32_t)(d - p) : 0;
                }
            }
            piece.blocks += r.blocks;
            piece.symbols += r.symbols;
            if (r.last_block_bit != UINT64_MAX) {
                piece.last_block_bit = r.last_block_bit;
                piece.last_block_at = base + r.last_block_out;
            }
            if (r.status == gi::ERR_OVERFLOW) {
                s = r.resume;
                stream_start = false;
                continue;
            }
            break;
        }
        piece.in_bit_end = r.end_bit;
        piece.status = r.status;
        piece.landed = r.status == gi::OK && r.landed >= 0;
        return r;
    }

    // Decoding from headers[s], landing from header to header: does the chain reach headers[j]
    // (1), end cleanly before it with a final block (0: an embedded Deflate stream, such as a
    // .gz inside a tar, whose headers are not this stream's), or break with an error (-1: damage)?
    int chain_outcome(const std::vector<gi::Start>& headers, size_t s, size_t j) {
        while (s < j) {
            gi::Start st = headers[s];
            for (;;) {
                gi::Result q = dec.run(in, size, st, headers.data() + s + 1, (int)(j - s), scratch.data(), (uint32_t)scratch.size(), false);
                if (q.status == gi::ERR_OVERFLOW) {
                    st = q.resume;
                    continue;
                }
                if (q.status == gi::OK && q.landed >= 0) {
                    s = s + 1 + q.landed;
                    break;
                }
                return q.status == gi::OK ? 0 : -1;
            }
        }
        return 1;
    }

    // Literals and matches in the block at bit, up to its end-of-block code or where it fails
    uint64_t block_symbols(uint64_t bit) {
        gi::Start st{bit, bit, gi::AT_BLOCK, -1};
        uint64_t n = 0;
        for (;;) {
            gi::Result q = dec.run(in, size, st, nullptr, 0, scratch.data(), (uint32_t)scratch.size(), false);
            n += q.symbols;
            if (q.status != gi::ERR_OVERFLOW || q.eob_bit != UINT64_MAX) return n;
            st = q.resume;
        }
    }

    // A block holds no more symbols than the stream's regular count: zlib fills each to it but
    // the last. A decode that ran off the end of the file through more than that went through
    // damage that decodes as plausible data (a stretch of zeros, say): from that block on, the
    // bytes are suspect. Without this they would pass for a truncated file's last exact bytes.
    void suspect_overfull_tail(Outcome& o, uint32_t regular) {
        if (!regular || !o.truncated || o.pieces.empty()) return;
        Piece& p = o.pieces.back();
        if (p.suspect || p.status != gi::ERR_INPUT || p.last_block_bit == UINT64_MAX || p.last_block_at > p.bytes.size()) return;
        uint64_t n = block_symbols(p.last_block_bit);
        if (n <= regular) return;
        Piece t;
        size_t at = p.last_block_at;
        t.bytes.assign(p.bytes.begin() + at, p.bytes.end());
        t.known.assign(p.known.begin() + at, p.known.end());
        t.var.assign(p.var.begin() + at, p.var.end());
        t.in_bit_start = p.last_block_bit;
        t.in_bit_end = p.in_bit_end;
        t.status = p.status;
        t.blocks = 1;
        t.symbols = (uint32_t)std::min<uint64_t>(n, UINT32_MAX);
        t.suspect = true;
        t.why = "its block holds " + std::to_string(n) + " symbols, more than the stream's regular " + std::to_string(regular) +
                ": the decode ran through damaged data to the end of the file";
        p.bytes.resize(at);
        p.known.resize(at);
        p.var.resize(at);
        p.in_bit_end = p.last_block_bit;
        p.status = gi::OK;
        p.blocks = p.blocks ? p.blocks - 1 : 0;
        p.symbols = p.symbols > t.symbols ? p.symbols - t.symbols : 0;
        if (p.bytes.empty()) {
            t.stream_start = p.stream_start;
            p = std::move(t);
        } else {
            o.pieces.push_back(std::move(t));
        }
        o.members.back().last_piece = o.pieces.size() - 1;
        for (std::string& note : o.notes)
            if (note == "the file ends inside the stream (truncated)")
                note = "the decode ran to the end of the file through damaged data: its last block holds more symbols than the stream's blocks do";
    }

    // the trusted output grows by a piece; the window keeps its last 32 KB (several pieces, when
    // blocks are small)
    void set_window(const Piece& p) {
        win_bytes.insert(win_bytes.end(), p.bytes.begin(), p.bytes.end());
        win_known.insert(win_known.end(), p.known.begin(), p.known.end());
        win_var.insert(win_var.end(), p.var.begin(), p.var.end());
        if (win_bytes.size() > gi::WINDOW) {
            win_bytes.erase(win_bytes.begin(), win_bytes.end() - gi::WINDOW);
            win_known.erase(win_known.begin(), win_known.end() - gi::WINDOW);
            win_var.erase(win_var.begin(), win_var.end() - gi::WINDOW);
        }
        win_valid = true;
    }

    void drop_window() {
        win_bytes.clear();
        win_known.clear();
        win_var.clear();
        win_valid = false;
    }

    // One pass over the file. `forced`: start bits of pieces known to be damaged from an earlier
    // pass. gzip: the stream starts after a member header of hlen bytes (0: damaged, start at
    // the first block header found) and ends with a trailer, more members may follow. Raw: the
    // stream starts at bit 0; the caller knows its size and CRC, if anyone does.
    Outcome rescue(const std::vector<gi::Start>& headers, bool gzip, uint64_t hlen, const std::set<uint64_t>& forced,
                   uint64_t expected_size = UINT64_MAX, bool has_crc = false, uint32_t expected_crc = 0) {
        Outcome o;
        drop_window();
        next_var = 1;
        bool stream_start = !gzip || hlen != 0;
        if (gzip && !hlen) o.notes.push_back("the gzip header is damaged: decoding starts at the first block header found");
        gi::Start cur = !gzip ? gi::Start{0, 0, gi::AT_BLOCK, -1}
                        : hlen  ? gi::Start{hlen * 8, hlen * 8, gi::AT_BLOCK, -1}
                                : headers[0];
        o.members.push_back({});
        if (!gzip) {
            Member& m = o.members.back();
            m.trailer = expected_size != UINT64_MAX;
            m.isize = (uint32_t)expected_size;
            m.crc = expected_crc;
            m.has_crc = has_crc;
            m.size_known = expected_size != UINT64_MAX;
        }
        size_t t0 = 0;  // headers[t0] is the first header after cur
        while (cur.bit < nbits) {
            while (t0 < headers.size() && headers[t0].bit <= cur.bit) t0++;
            Piece p;
            gi::Result r = decode_piece(cur, stream_start, headers.data() + t0, (int)(headers.size() - t0), p);
            stream_start = false;
            Member& m = o.members.back();
            m.last_piece = o.pieces.size();
            bool forced_bad = forced.count(p.in_bit_start) > 0;
            if (forced_bad) {
                p.suspect = true;
                p.why = "its symbol count differs from the stream's regular one";
            }
            uint64_t t = (r.end_bit + 7) / 8, next = gzip ? t + 8 : t;
            uint64_t nh = gzip && next < size ? gzip_header(in, size, next) : 0;
            // a final block is believed when the file ends after its trailer, another member
            // follows, or no block header is left: garbage decodes end in fake final blocks.
            // A raw stream is also believed when its output has the size the caller expects.
            bool real_end = next >= size || nh || t0 >= headers.size() ||
                            (!gzip && expected_size != UINT64_MAX && produced_so_far(o) + p.bytes.size() == expected_size);
            if (r.status == gi::OK && r.landed == 0 && !forced_bad) {  // landed on the next header: in step
                o.pieces.push_back(std::move(p));
                set_window(o.pieces.back());
                cur = headers[t0];
                continue;
            }
            if (r.status == gi::OK && r.landed > 0 && !forced_bad) {
                // passed over headers: real ones, missed because the decode went through damage,
                // or an embedded stream's, which this stream never lands on
                size_t j = t0 + r.landed;
                if (chain_outcome(headers, t0, j) == 0) {
                    o.pieces.push_back(std::move(p));
                    set_window(o.pieces.back());
                    cur = headers[j];
                    t0 = j;
                    continue;
                }
                p.why = "its decode passed over the next block header without landing on it";
            }
            if (r.status == gi::OK && r.landed < 0 && real_end && !forced_bad) {  // the member's last block
                o.pieces.push_back(std::move(p));
                set_window(o.pieces.back());
                if (gzip && t + 8 <= size) {
                    m.trailer = true;
                    m.has_crc = true;
                    m.size_known = true;
                    memcpy(&m.crc, in + t, 4);
                    memcpy(&m.isize, in + t + 4, 4);
                }
                if (o.input_end == 0) o.input_end = t;
                if (!gzip) break;
                if (nh) {
                    o.members.push_back({o.pieces.size(), o.pieces.size()});
                    cur = {(next + nh) * 8, (next + nh) * 8, gi::AT_BLOCK, -1};
                    stream_start = true;
                    drop_window();
                    continue;
                }
                if (next < size) o.notes.push_back(std::to_string(size - next) + " bytes after the last member were not gzip data");
                break;
            }
            if (r.status == gi::ERR_INPUT && t0 >= headers.size()) {
                // the last symbol may have been read from bits past the end: what one symbol can
                // write (258 bytes) is not trusted
                for (size_t i = p.bytes.size() > gi::MAX_SYMBOL_OUT ? p.bytes.size() - gi::MAX_SYMBOL_OUT : 0; i < p.bytes.size(); i++) {
                    p.known[i] = 0;
                    p.var[i] = 0;
                }
                o.pieces.push_back(std::move(p));
                o.notes.push_back("the file ends inside the stream (truncated)");
                o.truncated = true;
                break;
            }
            if (p.why.empty()) {
                p.why = r.status == gi::OK && r.landed < 0 ? "a final block was read where the data goes on"
                        : r.status == gi::ERR_INPUT      ? "the decode ran off the end of the file"
                                                         : "the decode hit invalid data";
            }
            // damaged: keep what was decoded, restart at the next header, trust no window
            p.suspect = true;
            o.pieces.push_back(std::move(p));
            drop_window();
            if (t0 < headers.size()) {
                cur = headers[t0];
                continue;
            }
            // no header left to restart at: perhaps a fixed-Huffman block
            gi::Start fx = fixed_restart(r.end_bit, nbits);
            if (fx.bit == UINT64_MAX) {
                o.notes.push_back("no block header after byte " + std::to_string(r.end_bit / 8) + ": the rest is lost");
                break;
            }
            o.notes.push_back("resynchronized inside a fixed-Huffman block at byte " + std::to_string(fx.bit / 8));
            cur = fx;
        }
        return o;
    }
};

// zlib fills every block with the same number of symbols (16,383 by default); other encoders
// have their habits too. Pieces of one block whose count stands out from a clear majority are
// damaged in a way landing cannot see. Returns their start bits.
std::set<uint64_t> symbol_count_anomalies(const Outcome& o, std::string& note, uint32_t& regular) {
    regular = 0;
    std::map<uint32_t, int> freq;
    int considered = 0;
    auto eligible = [&](size_t k) {
        const Piece& p = o.pieces[k];
        bool last = false;
        for (const Member& m : o.members) last |= k == m.last_piece;
        return p.blocks == 1 && p.landed && !last && !p.suspect;
    };
    for (size_t k = 0; k < o.pieces.size(); k++)
        if (eligible(k)) {
            freq[o.pieces[k].symbols]++;
            considered++;
        }
    std::set<uint64_t> out;
    if (considered < 10) return out;
    auto best = std::max_element(freq.begin(), freq.end(), [](auto& a, auto& b) { return a.second < b.second; });
    if (best->second < considered * 7 / 10) return out;
    regular = best->first;
    for (size_t k = 0; k < o.pieces.size(); k++)
        if (eligible(k) && o.pieces[k].symbols != best->first) out.insert(o.pieces[k].in_bit_start);
    if (!out.empty())
        note = std::to_string(out.size()) + " block(s) with a symbol count other than the stream's regular " +
               std::to_string(best->first) + " (" + std::to_string(best->second) + " of " + std::to_string(considered) + " blocks)";
    return out;
}


// ---------------------------------------------------------------- inferring the lost window

// A variable-order model of the exact bytes: for contexts of 1 to 5 bytes, how often each byte
// followed; probabilities blend from the longest context seen down, as PPM does. Counts live in
// one open-addressing table keyed by the full (order, context, byte): no collision can lend a
// context counts it never had. It learns from about 1 MB of exact output, taken from the pieces
// nearest the damage: the text most like what was lost.
struct Model {
    static constexpr int ORDERS = 5;
    static constexpr uint32_t BITS = 23;
    static constexpr uint64_t TRAIN = 1u << 20;
    std::vector<uint64_t> keys;
    std::vector<uint32_t> vals;
    std::vector<uint32_t> uni;
    uint64_t n = 0;
    bool empty = true;

    static uint64_t key(uint64_t ctx, int k, int c) {
        uint64_t x = ctx & ((1ull << (8 * k)) - 1);
        x = x * 0x9E3779B97F4A7C15ull + ((uint64_t)k << 52) + (uint64_t)(c + 1) * 0xC2B2AE3D27D4EB4Full;
        x ^= x >> 31;
        x *= 0xBF58476D1CE4E5B9ull;
        x ^= x >> 29;
        return x | 1;  // never 0: 0 marks an empty slot
    }
    void add(uint64_t k) {
        uint32_t mask = (1u << BITS) - 1, i = (uint32_t)(k >> 20) & mask;
        for (int probe = 0; probe < 64; probe++, i = (i + 1) & mask) {
            if (keys[i] == k) {
                vals[i]++;
                return;
            }
            if (!keys[i]) {
                keys[i] = k;
                vals[i] = 1;
                return;
            }
        }
    }
    uint32_t get(uint64_t k) const {
        uint32_t mask = (1u << BITS) - 1, i = (uint32_t)(k >> 20) & mask;
        for (int probe = 0; probe < 64; probe++, i = (i + 1) & mask) {
            if (keys[i] == k) return vals[i];
            if (!keys[i]) return 0;
        }
        return 0;
    }

    void learn(const Piece& p, uint64_t& budget) {
        const uint8_t* x = p.bytes.data();
        const uint8_t* kn = p.known.data();
        uint64_t ctx = 0;
        int run = 0;
        for (size_t i = 0; i < p.bytes.size() && budget; i++) {
            if (kn[i] != 1) {
                run = 0;
                ctx = 0;
                continue;
            }
            uni[x[i]]++;
            n++;
            budget--;
            for (int k = 1; k <= ORDERS && k <= run; k++) {
                add(key(ctx, k, x[i]));
                add(key(ctx, k, -1));
            }
            ctx = ctx << 8 | x[i];
            run++;
        }
    }

    void build(const std::vector<Piece>& pieces) {
        keys.assign(1u << BITS, 0);
        vals.assign(1u << BITS, 0);
        uni.assign(256, 0);
        // the pieces nearest a piece with lost bytes first
        std::vector<int64_t> dist(pieces.size(), INT64_MAX);
        for (size_t k = 0; k < pieces.size(); k++) {
            bool lost = false;
            for (size_t i = 0; i < pieces[k].var.size() && !lost; i += 64) lost = pieces[k].var[i] != 0;
            if (!lost) continue;
            for (size_t j = 0; j < pieces.size(); j++) dist[j] = std::min<int64_t>(dist[j], std::abs((int64_t)j - (int64_t)k));
        }
        std::vector<size_t> order(pieces.size());
        for (size_t k = 0; k < order.size(); k++) order[k] = k;
        std::stable_sort(order.begin(), order.end(), [&](size_t a, size_t b) { return dist[a] < dist[b]; });
        uint64_t budget = TRAIN;
        for (size_t k : order) {
            if (!budget) break;
            if (!pieces[k].suspect) learn(pieces[k], budget);
        }
        empty = n < 4096;
    }

    // log P(c | the bytes before), from the longest context known down (ctx: newest lowest)
    float lp(uint64_t ctx, int known, int c) const {
        double pr = (uni[c] + 1.0) / (n + 256.0);
        for (int k = 1; k <= ORDERS && k <= known; k++) {
            double t = get(key(ctx, k, -1));
            if (t == 0) break;  // a context never seen: longer ones were not either
            double e = get(key(ctx, k, c));
            pr = (e + 1.5 * pr) / (t + 1.5);
        }
        return (float)std::log(pr);
    }
};

struct Occ {
    uint32_t piece, pos;
};

// Infers the bytes of lost windows; returns how many output bytes got a value
uint64_t infer_window(std::vector<Piece>& pieces, std::string& note) {
    constexpr size_t MAX_OCC = 64;  // places per variable used for scoring
    constexpr int AHEAD = 6;        // bytes after the candidate that are scored with it
    // log-odds the best value must lead the second by; GR_INFER_MARGIN / GR_INFER_ROUNDS for experiments
    const char* em = getenv("GR_INFER_MARGIN");
    const char* er = getenv("GR_INFER_ROUNDS");
    const float MARGIN = em ? (float)atof(em) : 14.0f;
    const int ROUNDS = er ? atoi(er) : 60;
    // how far one place alone must favour a value, and how far no place may favour another;
    // GR_INFER_SOLO / _AGAINST for experiments, infer_sweep.ps1 measured them on code and logs
    const char* es = getenv("GR_INFER_SOLO");
    const char* ea = getenv("GR_INFER_AGAINST");
    const float SOLO = es ? (float)atof(es) : 6.0f;
    const float AGAINST = ea ? (float)atof(ea) : 2.0f;
    FILE* dbg = getenv("GR_INFER_DEBUG") ? fopen(getenv("GR_INFER_DEBUG"), "w") : nullptr;
    std::unordered_map<uint32_t, std::vector<Occ>> occ;
    uint64_t unknown = 0;
    for (uint32_t k = 0; k < pieces.size(); k++) {
        const Piece& p = pieces[k];
        for (uint32_t i = 0; i < p.bytes.size(); i++) {
            if (p.known[i] || !p.var[i]) continue;
            unknown++;
            auto& v = occ[p.var[i]];
            if (v.size() < MAX_OCC) v.push_back({k, i});
        }
    }
    if (occ.empty()) return 0;
    Model m;
    m.build(pieces);
    if (m.empty) return 0;
    std::unordered_map<uint32_t, int> value;  // resolved variables
    auto val = [&](uint32_t k, int64_t i, uint32_t self, int cand) -> int {
        const Piece& p = pieces[k];
        if (i < 0 || i >= (int64_t)p.bytes.size()) return -1;
        if (p.known[i] == 1) return p.bytes[i];
        uint32_t v = p.var[i];
        if (!v) return -1;
        if (v == self) return cand;
        auto it = value.find(v);
        return it == value.end() ? -1 : it->second;
    };
    std::vector<uint32_t> todo;
    for (auto& [v, list] : occ) todo.push_back(v);
    std::sort(todo.begin(), todo.end());
    std::vector<float> score(256), per;
    for (int round = 0; round < ROUNDS && !todo.empty(); round++) {
        std::vector<std::pair<uint32_t, int>> accepted;
        std::vector<uint32_t> left;
        for (uint32_t v : todo) {
            const auto& list = occ[v];
            // the distinct contexts this byte appears in: a line copied 40 times is one piece of evidence
            struct Ctx {
                uint64_t before;  // up to 6 bytes before, newest lowest
                int nbefore;
                int after[AHEAD];
                int nafter;
            };
            std::vector<Ctx> ctxs;
            for (const Occ& o : list) {
                Ctx c{0, 0, {}, 0};
                for (int q = 1; q <= Model::ORDERS; q++) {
                    int b = val(o.piece, (int64_t)o.pos - q, v, -1);
                    if (b < 0) break;
                    c.before |= (uint64_t)b << (8 * (q - 1));
                    c.nbefore = q;
                }
                for (int q = 1; q <= AHEAD; q++) {
                    int b = val(o.piece, (int64_t)o.pos + q, v, -1);
                    if (b < 0) break;
                    c.after[q - 1] = b;
                    c.nafter = q;
                }
                // Only contexts with known bytes on both sides count: a byte known only from what
                // precedes it is guessed as the commonest continuation, and measured, that misleads
                if (c.nbefore < 2 || c.nafter < 2) continue;
                // contexts that agree on the 3 bytes either side are one piece of evidence: what
                // lies further away says little about this byte, and copies repeat it
                bool dup = false;
                for (const Ctx& d : ctxs) {
                    int nb = std::min(3, std::min(d.nbefore, c.nbefore)), na = std::min(3, std::min(d.nafter, c.nafter));
                    if (std::min(3, d.nbefore) != std::min(3, c.nbefore) || std::min(3, d.nafter) != std::min(3, c.nafter)) continue;
                    if (((d.before ^ c.before) & ((1ull << (8 * nb)) - 1)) == 0 && std::equal(d.after, d.after + na, c.after)) {
                        dup = true;
                        break;
                    }
                }
                if (!dup) ctxs.push_back(c);
            }
            if (ctxs.empty()) {
                left.push_back(v);
                continue;
            }
            // Every byte is a candidate: measured, keeping only the bytes seen after the byte before
            // let a value never seen there lose without a contest, and the wrong one win. Many weak
            // contexts that agree are not strong evidence, since their errors are alike (hex digits
            // in a fingerprint, say): at least one context must favour the value on its own by SOLO,
            // and none may favour another by AGAINST.
            // per context, each candidate's score; the total is their sum
            per.assign(ctxs.size() * 256, 0.0f);
            for (int x = 0; x < 256; x++) {
                float sc = 0;
                for (size_t ci = 0; ci < ctxs.size(); ci++) {
                    const Ctx& c = ctxs[ci];
                    // P(x | before), then each byte after given what precedes it, x included
                    uint64_t ctx = c.before;
                    int kn = c.nbefore;
                    float s1 = m.lp(ctx, kn, x);
                    ctx = ctx << 8 | (uint64_t)x;
                    kn++;
                    for (int q = 0; q < c.nafter; q++) {
                        s1 += m.lp(ctx, kn, c.after[q]);
                        ctx = ctx << 8 | (uint64_t)c.after[q];
                        kn++;
                    }
                    per[ci * 256 + x] = s1;
                    sc += s1;
                }
                score[x] = sc;
            }
            float best = -1e30f, second = -1e30f;
            int bc = 0;
            for (int x = 0; x < 256; x++) {
                if (score[x] > best) {
                    second = best;
                    best = score[x];
                    bc = x;
                } else if (score[x] > second) {
                    second = score[x];
                }
            }
            bool favoured = false, against = false;
            for (size_t ci = 0; ci < ctxs.size(); ci++) {
                const float* row = &per[ci * 256];
                float other = -1e30f;
                for (int x = 0; x < 256; x++)
                    if (x != bc) other = std::max(other, row[x]);
                favoured |= row[bc] - other >= SOLO;
                against |= other - row[bc] >= AGAINST;
            }
            if (favoured && !against && best - second >= MARGIN) {
                accepted.push_back({v, bc});
                if (dbg) {
                    fprintf(dbg, "%d %u %d %.2f %.2f %d %zu |", round, v, bc, best, second, 256, ctxs.size());
                    for (size_t q = 0; q < ctxs.size() && q < 3; q++) {
                        const Ctx& c = ctxs[q];
                        fprintf(dbg, " [");
                        for (int z = c.nbefore - 1; z >= 0; z--) fprintf(dbg, "%02x", (unsigned)((c.before >> (8 * z)) & 0xff));
                        fprintf(dbg, "_");
                        for (int z = 0; z < c.nafter; z++) fprintf(dbg, "%02x", (unsigned)c.after[z]);
                        fprintf(dbg, "]");
                    }
                    std::vector<std::pair<float, int>> top;
                    for (int x = 0; x < 256; x++) top.push_back({score[x], x});
                    std::sort(top.rbegin(), top.rend());
                    fprintf(dbg, " |");
                    for (size_t q = 0; q < top.size() && q < 3; q++) fprintf(dbg, " %02x:%.1f", top[q].second, top[q].first);
                    fprintf(dbg, "\n");
                }
            } else {
                left.push_back(v);
            }
        }
        if (accepted.empty()) break;
        for (auto& [v, c] : accepted) value[v] = c;
        todo.swap(left);
    }
    if (dbg) fclose(dbg);
    uint64_t done = 0;
    for (Piece& p : pieces)
        for (size_t i = 0; i < p.bytes.size(); i++) {
            if (p.known[i] || !p.var[i]) continue;
            auto it = value.find(p.var[i]);
            if (it == value.end()) continue;
            p.bytes[i] = (uint8_t)it->second;
            p.known[i] = 2;
            done++;
        }
    if (done)
        note = std::to_string(done) + " of " + std::to_string(unknown) +
               " bytes that copied from data lost with a damaged block were inferred (" + std::to_string(value.size()) +
               " lost window bytes, from where their copies appear and the file's own statistics)";
    return done;
}

// ---------------------------------------------------------------- ASCII-mode transfers

// When nearly every LF has a CR before it (Unix to Windows in ASCII mode; in compressed data one
// LF in 256 does by chance), the input with one CR removed before each LF
// A file that ends in a long run of zeros (a crash while it was written, a preallocated file, a
// download that stopped) ends where the zeros begin: Deflate never writes 64 zero bytes in a
// row, and decoding them yields plausible garbage. Returns the bytes cut.
uint64_t cut_zero_tail(uint64_t& size, const std::vector<uint8_t>& in) {
    uint64_t z = size;
    while (z > 0 && in[z - 1] == 0) z--;
    if (size - z < 64) return 0;
    uint64_t cut = size - z;
    size = z;
    return cut;
}

bool undo_lf_to_crlf(std::vector<uint8_t>& in, uint64_t& size) {
    uint64_t lf = 0, crlf = 0;
    for (uint64_t i = 0; i < size; i++)
        if (in[i] == '\n') {
            lf++;
            crlf += i > 0 && in[i - 1] == '\r';
        }
    if (lf < 16 || crlf < 0.98 * lf) return false;
    uint64_t o = 0;
    for (uint64_t i = 0; i < size; i++) {
        if (in[i] == '\r' && i + 1 < size && in[i + 1] == '\n') continue;
        in[o++] = in[i];
    }
    std::fill(in.begin() + o, in.end(), 0);
    size = o;
    return true;
}

}  // namespace

struct gr_result {
    gr_stats stats{};
    std::vector<gr_piece> pieces;
    std::vector<std::string> why, notes;
    std::vector<const char*> note_ptrs;
};

extern "C" {

GR_API uint32_t gr_abi_version(void) { return GR_ABI_VERSION; }

static int rescue_common(const uint8_t* in_, uint64_t size, bool gzip, uint64_t expected_size, bool has_crc, uint32_t expected_crc,
                         const gr_options* opt, gr_sink sink, void* user, gr_result** result) {
    if (result) *result = nullptr;
    if (!in_ && size) return GR_ERR_ARG;
    uint8_t fill = opt && opt->size >= 5 ? opt->fill : 0;
    bool infer = !(opt && opt->size >= 6 && opt->no_infer);
    try {
        std::vector<uint8_t> in(size + 64, 0);  // zero padding: the readers look ahead
        if (size) memcpy(in.data(), in_, size);
        bool ascii_undone = undo_lf_to_crlf(in, size);
        uint64_t zero_tail = cut_zero_tail(size, in);
        Rescuer rs(in.data(), size);
        uint64_t hlen = gzip ? gzip_header(in.data(), size, 0) : 0;
        std::vector<gi::Start> headers = rs.find_headers(gzip ? (hlen ? hlen * 8 : 80) : 1);
        if (gzip && headers.empty() && !hlen) return GR_ERR_NO_DEFLATE;
        if (!gzip && size == 0) return GR_ERR_NO_DEFLATE;
        // first pass; a second one when blocks that stayed in step have the wrong symbol count
        Outcome o = rs.rescue(headers, gzip, hlen, {}, expected_size, has_crc, expected_crc);
        std::string anomaly_note;
        uint32_t regular = 0;
        std::set<uint64_t> anomalies = symbol_count_anomalies(o, anomaly_note, regular);
        if (!anomalies.empty()) {
            o = rs.rescue(headers, gzip, hlen, anomalies, expected_size, has_crc, expected_crc);
            o.notes.push_back(anomaly_note);
        }
        rs.suspect_overfull_tail(o, regular);
        if (ascii_undone) o.notes.push_back("the file had gone through an ASCII-mode transfer (every LF became CR LF): undone");
        if (zero_tail)
            o.notes.push_back("the file ends in " + std::to_string(zero_tail) +
                              " zero bytes (written by a crash or an unfinished download, not by gzip): the data ends where they begin");
        std::vector<Piece>& pieces = o.pieces;
        uint64_t inferred = 0;
        if (infer) {
            std::string note;
            inferred = infer_window(pieces, note);
            if (!note.empty()) o.notes.push_back(note);
        }
        gr_result* res = new gr_result;
        res->stats.input_size = size;
        res->stats.headers = (uint32_t)headers.size();
        res->stats.members = (uint32_t)o.members.size();
        res->stats.truncated = o.truncated;
        res->stats.header_damaged = gzip && hlen == 0;
        res->stats.crc_check = -1;
        res->stats.input_end = o.input_end;
        res->stats.inferred_bytes = inferred;
        res->stats.ascii_undone = ascii_undone;

        // The true length of each suspect piece: the trailer's size minus what is certain, spread
        // over the suspect pieces by their compressed size when there are several. ISIZE counts
        // modulo 4 GiB; the difference is believed when the damaged blocks could plausibly hold it.
        for (Member& m : o.members) {
            uint64_t good = 0, suspect_bits = 0, sus_decoded = 0;
            int nsus = 0;
            for (size_t k = m.first_piece; k <= m.last_piece && k < pieces.size(); k++) {
                const Piece& p = pieces[k];
                if (p.suspect) {
                    nsus++;
                    sus_decoded += p.bytes.size();
                    suspect_bits += p.in_bit_end > p.in_bit_start ? p.in_bit_end - p.in_bit_start : 0;
                } else {
                    good += p.bytes.size();
                }
            }
            uint32_t missing = m.isize - (uint32_t)good;
            if (m.trailer && nsus > 0 && missing <= 4 * sus_decoded + (64u << 20)) {
                uint64_t given = 0;
                int seen = 0;
                for (size_t k = m.first_piece; k <= m.last_piece && k < pieces.size(); k++) {
                    if (!pieces[k].suspect) continue;
                    seen++;
                    uint64_t bits = pieces[k].in_bit_end > pieces[k].in_bit_start ? pieces[k].in_bit_end - pieces[k].in_bit_start : 0;
                    pieces[k].true_len = seen == nsus ? missing - given : (suspect_bits ? (uint64_t)missing * bits / suspect_bits : 0);
                    given += pieces[k].true_len;
                }
            }
            if (m.trailer && nsus == 0 && !pieces.empty() && pieces[m.last_piece].status == gi::OK) {
                bool all_known = true;
                for (size_t k = m.first_piece; k <= m.last_piece; k++)
                    for (uint8_t kn : pieces[k].known) all_known &= kn == 1;
                if (all_known) {
                    uint32_t c = 0;
                    for (size_t k = m.first_piece; k <= m.last_piece; k++) c = crc32(pieces[k].bytes.data(), pieces[k].bytes.size(), c);
                    bool size_ok = !m.size_known || (uint32_t)good == m.isize;
                    if (m.has_crc) m.crc_check = c == m.crc && size_ok;
                    else if (m.size_known) m.crc_check = size_ok ? -1 : 0;
                    if (&m == &o.members.front()) res->stats.crc_check = m.crc_check;
                    if (m.size_known && (uint32_t)good != m.isize) {
                        res->stats.length_off = (int64_t)(int32_t)((uint32_t)good - m.isize);
                        o.notes.push_back("the output is " + std::to_string(-res->stats.length_off) +
                                          " bytes off the length the trailer gives: a block was damaged in a way that went "
                                          "unnoticed (the decoder fell back into step inside it); the CRC says where it is not");
                    }
                }
            }
        }
        // With damaged blocks in the stream its CRC cannot be checked, and "exact" means exact as
        // far as decoding can tell: measured on bit rot, a flipped bit that kept the decoder in
        // step left megabytes of a log wrong and unflagged.
        if (res->stats.crc_check == -1) {
            bool damaged = false;
            for (const Piece& p : pieces) damaged |= p.suspect;
            if (damaged)
                o.notes.push_back("with damaged blocks in it, the stream's CRC cannot be checked: bytes marked exact are exact as far as "
                                  "decoding can tell, and damage that kept the decoder in step (a flipped bit that changed a literal "
                                  "or a match length) would not show");
        }

        // GR_DUMP_VARS=<file>: the variable of every output byte (uint32, 0: none), for checking
        if (const char* dv = getenv("GR_DUMP_VARS")) {
            if (FILE* f = fopen(dv, "wb")) {
                for (const Piece& p : pieces) {
                    size_t len = p.suspect && p.true_len ? (size_t)p.true_len : p.bytes.size();
                    std::vector<uint32_t> v(len, 0);
                    for (size_t i = 0; i < std::min(len, p.var.size()); i++) v[i] = p.known[i] == 1 ? 0 : p.var[i];
                    fwrite(v.data(), 4, v.size(), f);
                }
                fclose(f);
            }
        }
        // the output: every piece, a suspect one cut or padded to its true length when known
        std::vector<uint8_t> buf, mask;
        uint64_t pos = 0;
        int rc = GR_OK;
        for (size_t k = 0; k < pieces.size(); k++) {
            Piece& p = pieces[k];
            size_t len = p.suspect && p.true_len ? (size_t)p.true_len : p.bytes.size();
            buf.assign(len, fill);
            mask.assign(len, GR_UNKNOWN);
            uint64_t known = 0;
            for (size_t i = 0; i < std::min(len, p.bytes.size()); i++) {
                if (!p.known[i]) {
                    res->stats.unknown_bytes++;
                    continue;
                }
                buf[i] = p.bytes[i];
                known++;
                if (p.suspect) {
                    mask[i] = GR_SUSPECT;
                    res->stats.suspect_bytes++;
                } else if (p.known[i] == 2) {
                    mask[i] = GR_INFERRED;
                } else {
                    mask[i] = GR_EXACT;
                    res->stats.exact_bytes++;
                }
            }
            if (len > p.bytes.size()) res->stats.unknown_bytes += len - p.bytes.size();
            res->why.push_back(p.why);
            res->pieces.push_back(gr_piece{pos, (uint64_t)len, (uint64_t)p.bytes.size(), known, p.in_bit_start / 8, p.in_bit_end / 8,
                                           p.suspect ? 1 : 0, p.status, p.blocks, p.symbols, nullptr});
            if (p.suspect) res->stats.damaged++;
            if (rc == GR_OK && sink && len && sink(user, buf.data(), mask.data(), len)) rc = GR_ERR_ABORTED;
            pos += len;
        }
        for (size_t k = 0; k < res->pieces.size(); k++) res->pieces[k].why = res->why[k].c_str();
        res->stats.output_size = pos;
        res->stats.pieces = (uint32_t)res->pieces.size();
        res->notes = o.notes;
        for (const std::string& n : res->notes) res->note_ptrs.push_back(n.c_str());
        if (result) *result = res;
        else gr_free(res);
        return rc;
    } catch (const std::bad_alloc&) {
        return GR_ERR_MEMORY;
    }
}

GR_API int gr_rescue(const uint8_t* in, uint64_t size, const gr_options* opt, gr_sink sink, void* user, gr_result** result) {
    return rescue_common(in, size, true, UINT64_MAX, false, 0, opt, sink, user, result);
}

GR_API int gr_rescue_raw(const uint8_t* in, uint64_t size, uint64_t expected_size, int has_crc, uint32_t expected_crc,
                         const gr_options* opt, gr_sink sink, void* user, gr_result** result) {
    return rescue_common(in, size, false, expected_size, has_crc != 0, expected_crc, opt, sink, user, result);
}

GR_API const gr_stats* gr_result_stats(const gr_result* r) { return r ? &r->stats : nullptr; }

GR_API uint32_t gr_result_pieces(const gr_result* r, const gr_piece** pieces) {
    if (!r) return 0;
    if (pieces) *pieces = r->pieces.data();
    return (uint32_t)r->pieces.size();
}

GR_API uint32_t gr_result_notes(const gr_result* r, const char* const** notes) {
    if (!r) return 0;
    if (notes) *notes = r->note_ptrs.data();
    return (uint32_t)r->note_ptrs.size();
}

GR_API void gr_free(gr_result* r) { delete r; }

}  // extern "C"
