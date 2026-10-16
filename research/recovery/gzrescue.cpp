// gzrescue - recover what a damaged gzip file still holds.
//
//   gzrescue <damaged.gz> <out> [--report out.json] [--mask out.mask] [--fill 0]
//
// gzip stops at the first damaged byte and everything after it is lost; worse, Deflate often
// does not notice damage at all and decodes garbage that falls back into step (Huffman codes
// resynchronize), so the error surfaces only in the final checksum, with no location.
//
// This tool knows where the blocks are. It finds every dynamic-block header in the file (the
// checks zlib applies to a header, after a cheap test on every bit position, from gpuinflate;
// each is confirmed by decoding on from it), then decodes block by block: a decode that
// starts at one header and lands exactly on the next is in step; one that errors, or lands on a
// later header having passed over the next one, went through damage. A block that stays in
// step can still hide damage: zlib puts the same number of symbols in every block (16,383 by
// default), so a block whose count differs from the stream's regular one is damaged too. A
// damaged block's output is kept but marked suspect, and decoding restarts at the next header,
// so only the damaged block itself is in doubt. Bytes after a restart that copy from a suspect
// block, and whatever copies from those in turn, are marked unknown (written as --fill): on
// ordinary data the window refreshes within a few KB; on very repetitive data (logs) the
// damage can taint everything after it. The suspect block's true length comes from the gzip
// trailer when there is one such block, so the offsets of everything after it stay right (a
// tar stays extractable).
//
// --mask writes one byte per output byte: 1 exact, 2 suspect (in a damaged block), 0 unknown.
#include "../../gpuinflate/inflate.cuh"

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
    bool trailer = false;
    uint32_t crc = 0, isize = 0;
    int crc_check = -1;  // 1 ok, 0 mismatch, -1 not checkable
};

struct Outcome {
    std::vector<Piece> pieces;
    std::vector<Member> members;
    std::vector<std::string> notes;
};

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

    void set_window(const Piece& p) {
        size_t n = std::min<size_t>(p.bytes.size(), gi::WINDOW);
        win_bytes.assign(p.bytes.end() - n, p.bytes.end());
        win_known.assign(p.known.end() - n, p.known.end());
        win_valid = true;
    }

    // One pass over the file. `forced`: start bits of pieces known to be damaged from an earlier pass.
    Outcome rescue(const std::vector<gi::Start>& headers, uint64_t hlen, const std::set<uint64_t>& forced) {
        Outcome o;
        win_valid = false;
        bool stream_start = hlen != 0;
        if (!hlen) o.notes.push_back("the gzip header is damaged: decoding starts at the first block header found");
        gi::Start cur = hlen ? gi::Start{hlen * 8, hlen * 8, gi::AT_BLOCK, -1} : headers[0];
        o.members.push_back({});
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
            uint64_t t = (r.end_bit + 7) / 8, next = t + 8;
            uint64_t nh = next < size ? gzip_header(in, size, next) : 0;
            // a final block is believed when the file ends after its trailer, another member
            // follows, or no block header is left: garbage decodes end in fake final blocks
            bool real_end = next >= size || nh || t0 >= headers.size();
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
                if (t + 8 <= size) {
                    m.trailer = true;
                    memcpy(&m.crc, in + t, 4);
                    memcpy(&m.isize, in + t + 4, 4);
                }
                if (nh) {
                    o.members.push_back({o.pieces.size(), o.pieces.size()});
                    cur = {(next + nh) * 8, (next + nh) * 8, gi::AT_BLOCK, -1};
                    stream_start = true;
                    win_valid = false;
                    continue;
                }
                if (next < size) o.notes.push_back(std::to_string(size - next) + " bytes after the last member were not gzip data");
                break;
            }
            if (r.status == gi::ERR_INPUT && t0 >= headers.size()) {
                o.pieces.push_back(std::move(p));
                o.notes.push_back("the file ends inside the stream (truncated)");
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
            win_valid = false;
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
    for (size_t k = 0; k < o.pieces.size(); k++) {
        const Piece& p = o.pieces[k];
        bool last = false;
        for (const Member& m : o.members) last |= k == m.last_piece;
        if (p.blocks != 1 || !p.landed || last || p.suspect) continue;
        freq[p.symbols]++;
        considered++;
    }
    std::set<uint64_t> out;
    if (considered < 10) return out;
    auto best = std::max_element(freq.begin(), freq.end(), [](auto& a, auto& b) { return a.second < b.second; });
    if (best->second < considered * 7 / 10) return out;
    for (size_t k = 0; k < o.pieces.size(); k++) {
        const Piece& p = o.pieces[k];
        bool last = false;
        for (const Member& m : o.members) last |= k == m.last_piece;
        if (p.blocks != 1 || !p.landed || last || p.suspect) continue;
        if (p.symbols != best->first) out.insert(p.in_bit_start);
    }
    if (!out.empty())
        note = std::to_string(out.size()) + " block(s) with a symbol count other than the stream's regular " +
               std::to_string(best->first) + " (" + std::to_string(best->second) + " of " + std::to_string(considered) + " blocks)";
    return out;
}

std::string json_escape(const std::string& s) {
    std::string o;
    for (char c : s) {
        if (c == '"' || c == '\\') o += '\\';
        o += c;
    }
    return o;
}

}  // namespace

int main(int argc, char** argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: gzrescue <damaged.gz> <out> [--report out.json] [--mask out.mask] [--fill 0]\n");
        return 2;
    }
    std::string report_path, mask_path;
    int fill = 0;
    for (int i = 3; i < argc; i++) {
        std::string a = argv[i];
        if (a == "--report" && i + 1 < argc) report_path = argv[++i];
        else if (a == "--mask" && i + 1 < argc) mask_path = argv[++i];
        else if (a == "--fill" && i + 1 < argc) fill = atoi(argv[++i]);
    }
    FILE* f = fopen(argv[1], "rb");
    if (!f) {
        fprintf(stderr, "cannot open %s\n", argv[1]);
        return 1;
    }
    fseek(f, 0, SEEK_END);
    uint64_t size = (uint64_t)_ftelli64(f);
    fseek(f, 0, SEEK_SET);
    std::vector<uint8_t> in(size + 64, 0);  // zero padding: the readers look ahead
    if (fread(in.data(), 1, size, f) != size) return 1;
    fclose(f);

    Rescuer rs(in.data(), size);
    uint64_t hlen = gzip_header(in.data(), size, 0);
    std::vector<gi::Start> headers = rs.find_headers(hlen ? hlen * 8 : 80);
    if (headers.empty() && !hlen) {
        fprintf(stderr, "no Deflate block could be recognised\n");
        return 1;
    }
    // first pass; a second one when blocks that stayed in step have the wrong symbol count
    Outcome o = rs.rescue(headers, hlen, {});
    std::string anomaly_note;
    std::set<uint64_t> anomalies = symbol_count_anomalies(o, anomaly_note);
    if (!anomalies.empty()) {
        o = rs.rescue(headers, hlen, anomalies);
        o.notes.push_back(anomaly_note);
    }
    std::vector<Piece>& pieces = o.pieces;
    std::vector<Member>& members = o.members;
    std::vector<std::string>& notes = o.notes;

    // The true length of each suspect piece: the trailer's size minus what is certain, spread
    // over the suspect pieces by their compressed size when there are several. ISIZE counts
    // modulo 4 GiB; the difference is believed when the damaged blocks could plausibly hold it.
    for (Member& m : members) {
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
        if (m.trailer && nsus == 0 && pieces[m.last_piece].status == gi::OK) {
            bool all_known = true;
            for (size_t k = m.first_piece; k <= m.last_piece; k++)
                for (uint8_t kn : pieces[k].known) all_known &= kn != 0;
            if (all_known) {
                uint32_t c = 0;
                for (size_t k = m.first_piece; k <= m.last_piece; k++) c = crc32(pieces[k].bytes.data(), pieces[k].bytes.size(), c);
                m.crc_check = c == m.crc && (uint32_t)good == m.isize;
                if ((uint32_t)good != m.isize)
                    notes.push_back("the output is " + std::to_string((int64_t)(int32_t)(m.isize - (uint32_t)good)) +
                                    " bytes off the length the trailer gives: a block was damaged in a way that went "
                                    "unnoticed (the decoder fell back into step inside it); the CRC says where it is not");
            }
        }
    }

    // the output: every piece, a suspect one cut or padded to its true length when known
    FILE* out = fopen(argv[2], "wb");
    if (!out) {
        fprintf(stderr, "cannot write %s\n", argv[2]);
        return 1;
    }
    FILE* mk = mask_path.empty() ? nullptr : fopen(mask_path.c_str(), "wb");
    std::vector<uint64_t> piece_offset(pieces.size()), piece_written(pieces.size());
    uint64_t pos = 0, exact_total = 0, unknown_total = 0, suspect_total = 0;
    std::vector<uint8_t> buf, mask;
    for (size_t k = 0; k < pieces.size(); k++) {
        Piece& p = pieces[k];
        piece_offset[k] = pos;
        size_t len = p.suspect && p.true_len ? (size_t)p.true_len : p.bytes.size();
        buf.assign(len, (uint8_t)fill);
        mask.assign(len, 0);
        for (size_t i = 0; i < std::min(len, p.bytes.size()); i++) {
            if (!p.known[i]) {
                unknown_total++;
                continue;
            }
            buf[i] = p.bytes[i];
            mask[i] = p.suspect ? 2 : 1;
            if (p.suspect) suspect_total++;
            else exact_total++;
        }
        if (len > p.bytes.size()) unknown_total += len - p.bytes.size();
        fwrite(buf.data(), 1, len, out);
        if (mk) fwrite(mask.data(), 1, len, mk);
        piece_written[k] = len;
        pos += len;
    }
    fclose(out);
    if (mk) fclose(mk);

    size_t nsus = 0;
    for (const Piece& p : pieces) nsus += p.suspect;
    printf("%s: %llu bytes in, %llu bytes out; %zu block header%s found\n", argv[1], (unsigned long long)size, (unsigned long long)pos,
           headers.size(), headers.size() == 1 ? "" : "s");
    printf("  %zu piece%s, %zu damaged, %zu member%s\n", pieces.size(), pieces.size() == 1 ? "" : "s", nsus, members.size(),
           members.size() == 1 ? "" : "s");
    printf("  %llu bytes exact, %llu in damaged blocks (partly right, from their start), %llu unknown\n",
           (unsigned long long)exact_total, (unsigned long long)suspect_total, (unsigned long long)unknown_total);
    for (size_t k = 0; k < pieces.size(); k++)
        if (pieces[k].suspect)
            printf("  damaged block at input byte %llu, output offset %llu: %s\n", (unsigned long long)(pieces[k].in_bit_start / 8),
                   (unsigned long long)piece_offset[k], pieces[k].why.c_str());
    for (const Member& m : members)
        if (m.crc_check >= 0) printf("  member CRC %s\n", m.crc_check ? "ok: the data is intact" : "MISMATCH: an undetectable error somewhere");
    for (const std::string& n : notes) printf("  note: %s\n", n.c_str());

    if (!report_path.empty()) {
        FILE* rp = fopen(report_path.c_str(), "w");
        if (rp) {
            fprintf(rp, "{\n \"input\": \"%s\",\n \"input_size\": %llu,\n \"output_size\": %llu,\n \"headers_found\": %zu,\n",
                    json_escape(argv[1]).c_str(), (unsigned long long)size, (unsigned long long)pos, headers.size());
            fprintf(rp, " \"pieces\": [\n");
            for (size_t k = 0; k < pieces.size(); k++) {
                const Piece& p = pieces[k];
                uint64_t kn = 0;
                for (uint8_t x : p.known) kn += x;
                fprintf(rp, "  {\"out_offset\": %llu, \"length\": %llu, \"decoded\": %zu, \"known\": %llu, \"suspect\": %s, "
                        "\"why\": \"%s\", \"blocks\": %u, \"symbols\": %u, \"in_byte_start\": %llu, \"in_byte_end\": %llu, \"status\": %d}%s\n",
                        (unsigned long long)piece_offset[k], (unsigned long long)piece_written[k], p.bytes.size(), (unsigned long long)kn,
                        p.suspect ? "true" : "false", json_escape(p.why).c_str(), p.blocks, p.symbols,
                        (unsigned long long)(p.in_bit_start / 8), (unsigned long long)(p.in_bit_end / 8), p.status,
                        k + 1 < pieces.size() ? "," : "");
            }
            fprintf(rp, " ],\n \"members\": [\n");
            for (size_t k = 0; k < members.size(); k++)
                fprintf(rp, "  {\"first_piece\": %zu, \"last_piece\": %zu, \"isize\": %s, \"crc_check\": %d}%s\n", members[k].first_piece,
                        members[k].last_piece, members[k].trailer ? std::to_string(members[k].isize).c_str() : "null", members[k].crc_check,
                        k + 1 < members.size() ? "," : "");
            fprintf(rp, " ],\n \"notes\": [");
            for (size_t k = 0; k < notes.size(); k++) fprintf(rp, "%s\"%s\"", k ? ", " : "", json_escape(notes[k]).c_str());
            fprintf(rp, "]\n}\n");
            fclose(rp);
        }
    }
    return 0;
}
