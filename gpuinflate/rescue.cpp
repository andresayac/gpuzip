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
#include <map>
#include <set>
#include <string>
#include <vector>

namespace {

constexpr uint64_t SCRATCH = 32u << 20;      // symbols decoded per run() call
constexpr uint64_t VERIFY_OUT = 256u << 10;  // a header is confirmed by decoding this much (or to the end) from it
constexpr uint32_t VERIFY_BLOCKS = 2;
constexpr uint64_t FIXED_SEARCH = 8ull << 23;  // bits: how far past an error to try resynchronizing in a fixed block

struct Piece {
    uint64_t in_bit_start = 0, in_bit_end = 0;
    std::vector<uint8_t> bytes, known;
    bool suspect = false;       // damaged somewhere inside
    std::string why;            // how the damage showed
    uint64_t true_len = 0;      // suspect: what its length must have been (0: unknown)
    bool stream_start = false;  // began at a gzip member's first block
    bool landed = false;        // ended by landing on the next header
    int status = gi::OK;
    uint32_t blocks = 0, symbols = 0;
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
    bool win_valid = false;

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
        gi::Result r;
        for (;;) {
            r = dec.run(in, size, s, targets, ntargets, scratch.data(), (uint32_t)scratch.size(), stream_start && piece.bytes.empty());
            size_t base = piece.bytes.size();
            piece.bytes.resize(base + r.out_len);
            piece.known.resize(base + r.out_len);
            for (uint32_t i = 0; i < r.out_len; i++) {
                uint16_t v = scratch[i];
                size_t p = base + i;
                if (v <= gi::REF) {
                    piece.bytes[p] = (uint8_t)v;
                    piece.known[p] = 1;
                    continue;
                }
                uint32_t d = v - gi::REF;
                if (d <= p) {
                    piece.bytes[p] = piece.bytes[p - d];
                    piece.known[p] = piece.known[p - d];
                } else if (win_valid && d - p <= win_bytes.size()) {
                    size_t q = win_bytes.size() - (d - p);
                    piece.bytes[p] = win_bytes[q];
                    piece.known[p] = win_known[q];
                } else {
                    piece.bytes[p] = 0;
                    piece.known[p] = 0;
                }
            }
            piece.blocks += r.blocks;
            piece.symbols += r.symbols;
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

    // the trusted output grows by a piece; the window keeps its last 32 KB (several pieces, when
    // blocks are small)
    void set_window(const Piece& p) {
        win_bytes.insert(win_bytes.end(), p.bytes.begin(), p.bytes.end());
        win_known.insert(win_known.end(), p.known.begin(), p.known.end());
        if (win_bytes.size() > gi::WINDOW) {
            win_bytes.erase(win_bytes.begin(), win_bytes.end() - gi::WINDOW);
            win_known.erase(win_known.begin(), win_known.end() - gi::WINDOW);
        }
        win_valid = true;
    }

    void drop_window() {
        win_bytes.clear();
        win_known.clear();
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
std::set<uint64_t> symbol_count_anomalies(const Outcome& o, std::string& note) {
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
    for (size_t k = 0; k < o.pieces.size(); k++)
        if (eligible(k) && o.pieces[k].symbols != best->first) out.insert(o.pieces[k].in_bit_start);
    if (!out.empty())
        note = std::to_string(out.size()) + " block(s) with a symbol count other than the stream's regular " +
               std::to_string(best->first) + " (" + std::to_string(best->second) + " of " + std::to_string(considered) + " blocks)";
    return out;
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
    uint8_t fill = opt && opt->size >= sizeof(gr_options) ? opt->fill : 0;
    try {
        std::vector<uint8_t> in(size + 64, 0);  // zero padding: the readers look ahead
        if (size) memcpy(in.data(), in_, size);
        Rescuer rs(in.data(), size);
        uint64_t hlen = gzip ? gzip_header(in.data(), size, 0) : 0;
        std::vector<gi::Start> headers = rs.find_headers(gzip ? (hlen ? hlen * 8 : 80) : 1);
        if (gzip && headers.empty() && !hlen) return GR_ERR_NO_DEFLATE;
        if (!gzip && size == 0) return GR_ERR_NO_DEFLATE;
        // first pass; a second one when blocks that stayed in step have the wrong symbol count
        Outcome o = rs.rescue(headers, gzip, hlen, {}, expected_size, has_crc, expected_crc);
        std::string anomaly_note;
        std::set<uint64_t> anomalies = symbol_count_anomalies(o, anomaly_note);
        if (!anomalies.empty()) {
            o = rs.rescue(headers, gzip, hlen, anomalies, expected_size, has_crc, expected_crc);
            o.notes.push_back(anomaly_note);
        }
        std::vector<Piece>& pieces = o.pieces;
        gr_result* res = new gr_result;
        res->stats.input_size = size;
        res->stats.headers = (uint32_t)headers.size();
        res->stats.members = (uint32_t)o.members.size();
        res->stats.truncated = o.truncated;
        res->stats.header_damaged = gzip && hlen == 0;
        res->stats.crc_check = -1;
        res->stats.input_end = o.input_end;

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
                    for (uint8_t kn : pieces[k].known) all_known &= kn != 0;
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
                mask[i] = p.suspect ? GR_SUSPECT : GR_EXACT;
                known++;
                if (p.suspect) res->stats.suspect_bytes++;
                else res->stats.exact_bytes++;
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
