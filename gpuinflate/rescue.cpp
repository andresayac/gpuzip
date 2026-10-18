// rescue.cpp - gzrescue: recover what a damaged gzip file still holds. See rescue.h, and
// research/recovery/README.md for the method and what it recovers.
//
// 1. Every dynamic-block header in the file is found (the cheap test on every bit position and
//    zlib's header checks from the finder, each candidate confirmed by decoding two blocks).
// 2. Decoding goes block by block, landing: a decode from one header that lands exactly on the
//    next is in step; one that hits invalid data, runs off the end, reads a "final" block where
//    data goes on, or passes over the next header went through damage. Headers of Deflate
//    streams embedded in the data (a .gz inside a tar) are told apart: their chain of blocks
//    ends on its own instead of joining the stream's, or runs into the header of the stored
//    block that holds it.
// 3. Encoders end blocks by habit: zlib at 16,383 symbols (2^k - 1), pigz too but at the end of
//    each chunk, GNU gzip at 32,767 or a multiple of 4,096. A block in step with a count its
//    encoder never writes was damaged in a way landing cannot see: a second pass flags it (not
//    in a member whose CRC checks). Such damage, with no run of zeros to account for it, or a
//    broken block that one flipped bit mends, or a CR-stripped transfer, or a CRC that fails with
//    every block in step, is of a kind that elsewhere may change nothing a decoder sees: with no
//    CRC to check, nothing is vouched for.
// 4. A damaged block is kept but flagged; decoding restarts at the next header; bytes that copy
//    from a damaged block, and whatever copies from those, are unknown.
// 4b. A member decoded as one piece whose CRC fails though it decoded in step was damaged where
//    the decoder fell back in step: at a run of zeros or 0xFF bytes in its data (they decode as
//    valid codes), or, with no such run, anywhere in it. It is flagged in another pass. (With
//    several pieces nothing says which hold the damage: all are left as they are.)
// 4c. Within a damaged piece the damage begins at the first such run, when the decode from a
//    start in step broke only at it or after: what was decoded from the input before it is exact,
//    the rest suspect. A truncated decode that ran through such a run is suspect from it on.
// 5. The damaged block's true length comes from the trailer when there is one such block.
// 6. The unknown bytes are inferred: each is a copy of one byte of the window that was lost, and
//    every copy of it appears somewhere in the output with bytes around it. A model of the file's
//    exact bytes (contexts of 1 to 5 bytes) scores each possible value over all those places; the
//    values it is sure of are kept, marked inferred, and serve as context for the next round.
// 7. Transfers in ASCII mode are undone: LF to CR LF exactly; CR LF to LF, in a stream with its
//    own CRC (a ZIP entry), by putting a CR back before the LFs where that makes the CRC right.
// 8. The memory a rescue takes does not grow with its output. A pass decodes piece by piece
//    through a window, keeping the first HOLD bytes of output and, of the rest, only what step 6
//    needs (the places of each lost window byte with the bytes around them, and the exact bytes
//    near the damage the model learns from); the rest is decoded again, the same way, as it is
//    written out. The header search and the scoring of step 6 run in parallel.
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
#include <atomic>
#include <bit>
#include <exception>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <stdexcept>
#include <string>
#include <thread>
#include <unordered_map>
#include <vector>

namespace {

constexpr uint64_t SCRATCH = 32u << 20;      // symbols decoded per run() call
constexpr uint64_t VERIFY_OUT = 256u << 10;  // a header is confirmed by decoding this much (or to the end) from it
constexpr uint32_t VERIFY_BLOCKS = 2;
constexpr uint64_t FIXED_SEARCH = 8ull << 23;  // bits: how far past an error to try resynchronizing in a fixed block
constexpr uint64_t RUN = 16;                    // bytes of 0x00 or 0xFF in a row that mark where damage begins
constexpr uint64_t MAX_SYMBOL_BITS = 48;        // one symbol: a 15-bit length code, 5 extra bits, a 15-bit distance code, 13
constexpr uint64_t HOLD = 128ull << 20;         // output bytes a pass keeps in memory; the rest is decoded again to be written
constexpr uint32_t SLICE = 1u << 20;            // symbols resolved between handing bytes on
constexpr uint64_t TRIM = 1u << 20;             // bytes the tail of a piece sheds at a time
constexpr size_t MAX_OCC = 64;                  // places per lost window byte its value is inferred from
constexpr uint64_t TRAIN = 1u << 20;            // exact bytes the model of the file learns from
constexpr uint64_t CR_BUDGET = 1ull << 30;      // input bytes the search for a stream's lost CRs decodes, at most
constexpr uint64_t FLIP_BITS = 16384;           // how far before where a decode broke a flipped bit is looked for
constexpr int FLIP_PIECES = 8;                  // damaged pieces looked at for one

// How the stream's encoder ends its blocks (symbol_count_anomalies): GNU gzip at 32,767 symbols
// or a multiple of 4,096; zlib at 2^k - 1 (pigz too, and at the end of its chunks, a piece of
// several blocks when marked); one count for most blocks; or no habit seen.
struct Habit {
    enum Kind { NONE, GNU, ZLIB, ONE } kind = NONE;
    uint32_t regular = 0;  // the most symbols a block of the stream holds (0: no habit)
    // a piece of `blocks` blocks (its last ends a pigz chunk, when several) and `symbols` symbols
    bool fits(uint32_t symbols, uint32_t blocks) const {
        switch (kind) {
            case GNU: return symbols == 32767 || (symbols < 32768 && symbols % 4096 == 0);
            case ZLIB: return symbols == regular || blocks > 1;
            case ONE: return symbols == regular;
            default: return true;
        }
    }
};

// Workers for the parts that run in parallel; GR_THREADS for experiments (results do not depend on it)
unsigned threads() {
    static unsigned n = [] {
        const char* e = getenv("GR_THREADS");
        unsigned t = e ? (unsigned)atoi(e) : std::thread::hardware_concurrency();
        return std::max(1u, std::min(t, 64u));
    }();
    return n;
}

// fn(i, worker) for every i in [0, n), each once, on up to threads() workers (and max_workers).
// An exception in a worker (out of memory) stops the others and is thrown here.
template <class F>
void parallel_for(size_t n, F fn, unsigned max_workers = 64) {
    unsigned nt = (unsigned)std::min<size_t>(std::min(threads(), max_workers), n);
    if (nt <= 1) {
        for (size_t i = 0; i < n; i++) fn(i, 0u);
        return;
    }
    std::atomic<size_t> next{0};
    std::exception_ptr failed;
    std::mutex m;
    std::vector<std::thread> pool;
    for (unsigned w = 0; w < nt; w++)
        pool.emplace_back([&, w] {
            try {
                for (size_t i; (i = next.fetch_add(1)) < n;) fn(i, w);
            } catch (...) {
                std::lock_guard<std::mutex> g(m);
                if (!failed) failed = std::current_exception();
                next = n;
            }
        });
    for (std::thread& t : pool) t.join();
    if (failed) std::rethrow_exception(failed);
}

// One place a variable appears (output byte pos): what lies around it in its piece, from 5
// bytes before to 6 after. A cell is a byte (0-255), another variable (VAR + its number) or
// nothing known (NONE).
struct Occ {
    static constexpr uint32_t NONE = 0xFFFFFFFFu, VAR = 0x100;
    static constexpr int BEFORE = 5, AFTER = 6, CELLS = BEFORE + AFTER;
    uint64_t pos;
    uint32_t cell[CELLS];  // i < BEFORE: i + 1 bytes before; else i - BEFORE + 1 after
};
// bytes of a piece handed on only once its end is settled: what its last symbol may write, and
// the bytes after a place that its Occ records
constexpr uint64_t DELAY = gi::MAX_SYMBOL_OUT + Occ::AFTER;

// Places, packed in the order recorded (a heavily damaged file records over a hundred million).
// Most cells around the place of variable v are the variables of the window bytes next to v's,
// copied along with it: q bytes before v lies v + q, q after it v - q. Where another copy begins,
// its variables run on the same way. So a cell is a 2-bit code: nothing known, regular (the cell
// nearer the place run on by one when that is a variable, else v + q or v - q), a byte, or
// another variable, and only a byte or another variable has a payload. A place, at an offset in
// the store, is: how far back the same variable's place before it lies (varint, 0: none), its
// output offset less its chunk's first (varint; places come in output order), then its cells.
// Short form, one byte with low bits SHORT: the nb cells nearest before the place and the na
// nearest after are regular, the others unknown. Else 3 bytes of codes (cell i at bit 2 + 2i),
// then the payloads in cell order: a byte as itself, another variable as its difference from the
// regular one modulo 2^16 (2 bytes: the variables around one place are of the same lost window,
// fewer than WINDOW apart). A place never straddles two chunks.
struct OccStore {
    static constexpr int CHUNK_BITS = 16;
    static constexpr uint64_t CHUNK = 1ull << CHUNK_BITS;
    static constexpr uint64_t MOST = 10 + 10 + 3 + 2 * Occ::CELLS;  // bytes a place may take
    static constexpr uint8_t SHORT = 1;
    enum : uint32_t { NOTHING, REGULAR, BYTE, OTHER };
    std::vector<std::unique_ptr<uint8_t[]>> chunks;
    std::vector<uint64_t> base;  // by chunk: the output offset of its first place
    uint64_t end = 0;            // where the next place goes

    // the place of variable self whose place before is at prev (UINT64_MAX: none); returns where it went
    uint64_t add(const Occ& o, uint32_t self, uint64_t prev) {
        if (end == chunks.size() * CHUNK || CHUNK - end % CHUNK < MOST) {
            end = chunks.size() * CHUNK;
            chunks.emplace_back(new uint8_t[CHUNK]);
            base.push_back(o.pos);
        }
        uint64_t r = end, b = base[r >> CHUNK_BITS];
        if (o.pos < b) throw std::runtime_error("places were recorded out of order");
        uint8_t* p = at(r);
        uint8_t* w = put(p, prev == UINT64_MAX ? 0 : r - prev);
        w = put(w, o.pos - b);
        w = put_cells(w, o, self);
        end = r + (uint64_t)(w - p);
        return r;
    }
    // where the place before the one at r lies (UINT64_MAX: none)
    uint64_t prev(uint64_t r) const {
        const uint8_t* p = at(r);
        uint64_t back = get(p);
        return back ? r - back : UINT64_MAX;
    }
    // the place at r, of variable self
    void read(uint64_t r, uint32_t self, Occ& o) const {
        const uint8_t* p = at(r);
        get(p);
        o.pos = base[r >> CHUNK_BITS] + get(p);
        if ((p[0] & 3) == SHORT) {
            int nb = p[0] >> 2 & 7, na = p[0] >> 5;
            for (int i = 0; i < Occ::CELLS; i++)
                o.cell[i] = i < nb || (i >= Occ::BEFORE && i < Occ::BEFORE + na) ? var(self, regular(i)) : Occ::NONE;
            return;
        }
        uint32_t codes = (p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16) >> 2;
        p += 3;
        uint16_t d[Occ::CELLS];  // a variable's difference from self
        bool isvar[Occ::CELLS];
        for (int i = 0; i < Occ::CELLS; i++) {
            uint32_t code = codes >> 2 * i & 3;
            isvar[i] = code == REGULAR || code == OTHER;
            if (code == NOTHING) o.cell[i] = Occ::NONE;
            else if (code == BYTE) o.cell[i] = *p++;
            else {
                d[i] = predict(i, isvar, d);
                if (code == OTHER) {
                    d[i] = (uint16_t)(d[i] + (p[0] | p[1] << 8));
                    p += 2;
                }
                o.cell[i] = var(self, d[i]);
            }
        }
    }
    // the cells of the place at r, of variable self, set again. Only the cells farthest from the
    // place are ever cleared (those past a piece's ends): the others are regular as they were, so
    // the place does not grow.
    void rewrite(uint64_t r, const Occ& o, uint32_t self) {
        uint8_t* p = at(r);
        const uint8_t* q = p;
        get(q);
        get(q);
        p += q - p;
        size_t was = 1;
        if ((p[0] & 3) != SHORT) {
            uint32_t codes = (p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16) >> 2;
            was = 3;
            for (int i = 0; i < Occ::CELLS; i++) was += (codes >> 2 * i & 3) == BYTE ? 1 : (codes >> 2 * i & 3) == OTHER ? 2 : 0;
        }
        uint8_t buf[MOST];
        size_t n = (size_t)(put_cells(buf, o, self) - buf);
        if (n > was) throw std::runtime_error("a place grew when cells were cleared");
        memcpy(p, buf, n);
    }

  private:
    uint8_t* at(uint64_t r) const { return chunks[r >> CHUNK_BITS].get() + (r & (CHUNK - 1)); }
    static uint8_t* put(uint8_t* w, uint64_t x) {
        for (; x >= 0x80; x >>= 7) *w++ = (uint8_t)(x | 0x80);
        *w++ = (uint8_t)x;
        return w;
    }
    static uint64_t get(const uint8_t*& p) {
        uint64_t x = 0;
        for (int s = 0;; s += 7) {
            uint8_t b = *p++;
            x |= (uint64_t)(b & 0x7f) << s;
            if (!(b & 0x80)) return x;
        }
    }
    // cell i's regular neighbour, as a difference from the place's own variable
    static uint16_t regular(int i) { return (uint16_t)(i < Occ::BEFORE ? i + 1 : Occ::BEFORE - 1 - i); }
    // the variable cell i is regular for, from the differences of the cells nearer the place
    static uint16_t predict(int i, const bool* isvar, const uint16_t* d) {
        if (i == 0 || i == Occ::BEFORE || !isvar[i - 1]) return regular(i);
        return (uint16_t)(i < Occ::BEFORE ? d[i - 1] + 1 : d[i - 1] - 1);
    }
    // the variable that differs from self by d (as a cell held it before: d is all there is)
    static uint32_t var(uint32_t self, uint16_t d) { return Occ::VAR + self + (uint32_t)(int32_t)(int16_t)d; }
    static uint8_t* put_cells(uint8_t* w, const Occ& o, uint32_t self) {
        uint16_t d[Occ::CELLS];
        bool isvar[Occ::CELLS];
        for (int i = 0; i < Occ::CELLS; i++) {
            isvar[i] = o.cell[i] != Occ::NONE && o.cell[i] >= Occ::VAR;
            d[i] = (uint16_t)(o.cell[i] - Occ::VAR - self);
        }
        int nb = 0, na = 0;
        while (nb < Occ::BEFORE && isvar[nb] && d[nb] == regular(nb)) nb++;
        while (na < Occ::AFTER && isvar[Occ::BEFORE + na] && d[Occ::BEFORE + na] == regular(Occ::BEFORE + na)) na++;
        bool rest = false;
        for (int i = nb; i < Occ::BEFORE; i++) rest |= o.cell[i] != Occ::NONE;
        for (int i = Occ::BEFORE + na; i < Occ::CELLS; i++) rest |= o.cell[i] != Occ::NONE;
        if (!rest) {
            *w++ = (uint8_t)(SHORT | nb << 2 | na << 5);
            return w;
        }
        uint8_t* codes = w;
        w += 3;
        uint32_t cw = 0;
        for (int i = 0; i < Occ::CELLS; i++) {
            uint32_t c = o.cell[i], code;
            if (c == Occ::NONE) code = NOTHING;
            else if (!isvar[i]) {
                code = BYTE;
                *w++ = (uint8_t)c;
            } else {
                uint16_t e = (uint16_t)(d[i] - predict(i, isvar, d));
                code = e ? OTHER : REGULAR;
                if (e) {
                    w[0] = (uint8_t)e;
                    w[1] = (uint8_t)(e >> 8);
                    w += 2;
                }
            }
            cw |= code << (2 + 2 * i);
        }
        codes[0] = (uint8_t)cw;
        codes[1] = (uint8_t)(cw >> 8);
        codes[2] = (uint8_t)(cw >> 16);
        return w;
    }
};

// The variables a pass met: for each, how many bytes copy it and its first MAX_OCC places, a list
// back from its last in the store. Found by an open-addressing table: a file damaged in every
// block has millions of them.
struct VarTable {
    struct Var {
        uint32_t var;
        uint16_t nocc = 0, kept = 0;  // its places met (at most MAX_OCC), and of them those in occs
        uint64_t count = 0;
        uint64_t last = UINT64_MAX;  // where its last place is in occs
    };
    std::vector<Var> vars;        // in the order first met
    std::vector<uint32_t> slots;  // an index in vars + 1; 0: empty
    OccStore occs;

    static uint32_t hash(uint32_t v) { return (uint32_t)(((uint64_t)v * 0x9E3779B97F4A7C15ull) >> 32); }
    // the index of variable v, UINT32_MAX when it was not met
    uint32_t find(uint32_t v) const {
        if (slots.empty()) return UINT32_MAX;
        uint32_t mask = (uint32_t)slots.size() - 1;
        for (uint32_t i = hash(v) & mask;; i = (i + 1) & mask) {
            uint32_t k = slots[i];
            if (!k) return UINT32_MAX;
            if (vars[k - 1].var == v) return k - 1;
        }
    }
    // the index of variable v, which is added when new
    uint32_t get(uint32_t v) {
        if (2 * (vars.size() + 1) > slots.size()) {
            std::vector<uint32_t> grown(slots.empty() ? 1024 : 2 * slots.size(), 0);
            uint32_t mask = (uint32_t)grown.size() - 1;
            for (size_t k = 0; k < vars.size(); k++) {
                uint32_t i = hash(vars[k].var) & mask;
                while (grown[i]) i = (i + 1) & mask;
                grown[i] = (uint32_t)k + 1;
            }
            slots.swap(grown);
        }
        uint32_t mask = (uint32_t)slots.size() - 1;
        for (uint32_t i = hash(v) & mask;; i = (i + 1) & mask) {
            uint32_t k = slots[i];
            if (!k) {
                vars.push_back({v});
                slots[i] = (uint32_t)vars.size();
                return (uint32_t)vars.size() - 1;
            }
            if (vars[k - 1].var == v) return k - 1;
        }
    }
    // Does variable d have a place that agrees with o on the 3 cells either side? Scoring counts
    // such places once ("one piece of evidence"), whatever lies further out: a later one adds
    // nothing, and the variables only it holds around it are never looked at.
    bool alike(uint32_t d, const Occ& o) const {
        const Var& x = vars[d];
        Occ e;
        uint64_t r = x.last;
        for (uint32_t i = 0; i < x.kept; i++, r = occs.prev(r)) {
            occs.read(r, x.var, e);
            if (e.cell[0] == o.cell[0] && e.cell[1] == o.cell[1] && e.cell[2] == o.cell[2] && e.cell[5] == o.cell[5] &&
                e.cell[6] == o.cell[6] && e.cell[7] == o.cell[7])
                return true;
        }
        return false;
    }
    // a place of variable d: kept, or with `drop_alike` only counted when it is alike an earlier one
    void add_place(uint32_t d, const Occ& o, bool drop_alike = false) {
        Var& x = vars[d];
        x.nocc++;
        if (drop_alike && alike(d, o)) return;
        x.last = occs.add(o, x.var, x.last);
        x.kept++;
    }
    // fn(r, place) for each place of variable d in output order, r where it is in occs
    template <class Fn>
    void each_place(uint32_t d, Fn&& fn) const {
        const Var& x = vars[d];
        uint64_t at[MAX_OCC];
        uint64_t r = x.last;
        for (uint32_t i = x.kept; i-- > 0; r = occs.prev(r)) at[i] = r;
        Occ o;
        for (uint32_t i = 0; i < x.kept; i++) {
            occs.read(at[i], x.var, o);
            fn(at[i], o);
        }
    }
};

// The values inferred for lost window bytes, by variable
struct Values {
    const VarTable* vt = nullptr;
    std::vector<int16_t> value;  // by index in vt->vars; -1: none
    int get(uint32_t var) const {
        uint32_t d = vt ? vt->find(var) : UINT32_MAX;
        return d == UINT32_MAX ? -1 : value[d];
    }
};

// A piece of the output: what one decode produced, or part of it
struct Piece {
    uint64_t in_bit_start = 0, in_bit_end = 0;
    gi::Start start{UINT64_MAX, UINT64_MAX, gi::AT_BLOCK, -1};  // where its decode began
    uint64_t len = 0;           // bytes decoded
    // The bytes, when held: a pass keeps the first `hold` bytes of output in memory and decodes
    // the rest again to write it out. known: 0 unknown, 1 exact, 2 inferred; uvar: for each
    // unknown byte in turn, the window byte it copies (0: none)
    bool held = false;
    std::vector<uint8_t> bytes, known;
    std::vector<uint32_t> uvar;
    size_t src = 0;             // its bytes are those of decode src (Outcome::sources) from src_off on
    uint64_t src_off = 0;
    uint64_t exact = 0;         // bytes known exactly (0 for a part split off a decode: not counted)
    bool all_exact = true;      // every byte decoded exactly
    uint32_t var_base = 0;      // the first variable of the lost window before it (0: none)
    bool suspect = false;       // damaged somewhere inside
    std::string why;            // how the damage showed
    uint64_t true_len = 0;      // suspect: what its length must have been (0: unknown)
    bool stream_start = false;  // began at a gzip member's first block
    bool landed = false;        // ended by landing on the next header
    bool anchored = false;      // its start is a real block start: the decode before it was in step, or it landed
    bool by_crc = false;        // flagged because the member's CRC or length failed, every block in step
    bool length_exact = false;  // suspect, but its decode stayed in step: its length is right
    int status = gi::OK;
    uint32_t blocks = 0, symbols = 0;
    uint64_t last_block_bit = UINT64_MAX;  // header of the last block it started
    size_t last_block_at = 0;              // where that block's output begins
};

// How a pass decoded one piece, to decode it again the same way
struct Source {
    gi::Start start{UINT64_MAX, UINT64_MAX, gi::AT_BLOCK, -1};
    bool stream_start = false;
    size_t t0 = 0;              // its targets: the headers from t0 on
    uint32_t var_base = 0;
    uint64_t out = 0, len = 0;  // where its output begins, and how long it is
    bool window_after = false;  // the window after it ends with its bytes (else there is none)
    bool blank_tail = false;    // the file ends inside it: its last symbol's bytes are unknown
    uint64_t lost64 = UINT64_MAX;  // the first offset in it, a multiple of 64, of a byte that copies a variable
};

struct Member {
    size_t first_piece = 0, last_piece = 0;
    bool trailer = false;  // the size (and perhaps the CRC) of the original is known
    bool has_crc = false, size_known = false;
    uint32_t crc = 0, isize = 0;
    uint32_t out_crc = 0;  // CRC-32 of the bytes decoded for it
    int crc_check = -1;    // 1 ok, 0 mismatch, -1 not checkable
};

// The last 32 KB of trusted output, for references that reach before a piece's start
struct Window {
    std::vector<uint8_t> bytes, known;
    std::vector<uint32_t> var;
    bool valid = false;
};

// The exact bytes of a decode from its start, up to TRAIN of them, as runs at their offsets:
// what the model learns from a piece that is not held
using Learned = std::vector<std::pair<uint64_t, std::vector<uint8_t>>>;

// What inferring the lost windows needs from a pass's output, gathered as it goes by
struct Records {
    VarTable vt;  // each variable met: how many bytes copy it, and its first MAX_OCC places
    // The ranges [a, b) of bytes that copy a variable, in decodes that may be split later (damaged
    // ones, and the last), by decode: what parts of them have lost bytes
    std::map<size_t, std::vector<std::pair<uint64_t, uint64_t>>> runs;
    std::map<size_t, Learned> learned;                    // by decode, for decodes not held
};

struct Outcome {
    std::vector<Piece> pieces;
    std::vector<Member> members;
    std::vector<std::string> notes;
    bool truncated = false;
    uint64_t input_end = 0;       // byte after the first member's last block
    std::vector<Source> sources;  // the pass's decodes, in order
    Records rec;
    uint64_t total = 0;           // output bytes
    uint64_t held_bytes = 0;
    size_t unheld = SIZE_MAX;     // the first decode whose bytes are not held (SIZE_MAX: all are)
    Window resume;                // the window before it
    bool too_big = false;         // the output grew past max_output: the pass stopped
};

uint64_t bits64(const uint8_t* in, uint64_t bit) {
    uint64_t lo, hi;
    memcpy(&lo, in + (bit >> 3), 8);
    memcpy(&hi, in + (bit >> 3) + 8, 8);
    int off = (int)(bit & 7);
    return off ? (lo >> off) | (hi << (64 - off)) : lo;
}

// CRC-32 (gzip's), eight bytes a step
uint32_t crc32(const uint8_t* p, size_t n, uint32_t crc = 0) {
    static const auto table = [] {
        std::vector<uint32_t> t(8 * 256);
        for (uint32_t i = 0; i < 256; i++) {
            uint32_t c = i;
            for (int k = 0; k < 8; k++) c = c & 1 ? 0xEDB88320u ^ (c >> 1) : c >> 1;
            t[i] = c;
        }
        for (uint32_t i = 0; i < 256; i++)
            for (int s = 1; s < 8; s++) t[s * 256 + i] = t[(s - 1) * 256 + i] >> 8 ^ t[t[(s - 1) * 256 + i] & 0xff];
        return t;
    }();
    const uint32_t* t = table.data();
    crc = ~crc;
    for (; n >= 8; n -= 8, p += 8) {
        uint32_t a, b;
        memcpy(&a, p, 4);
        memcpy(&b, p + 4, 4);
        a ^= crc;
        crc = t[7 * 256 + (a & 0xff)] ^ t[6 * 256 + (a >> 8 & 0xff)] ^ t[5 * 256 + (a >> 16 & 0xff)] ^ t[4 * 256 + (a >> 24)] ^
              t[3 * 256 + (b & 0xff)] ^ t[2 * 256 + (b >> 8 & 0xff)] ^ t[1 * 256 + (b >> 16 & 0xff)] ^ t[b >> 24];
    }
    while (n--) crc = t[(crc ^ *p++) & 0xff] ^ (crc >> 8);
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

// Does decoding from s go on cleanly for long enough to trust it? scratch: VERIFY_OUT symbols
bool verify_at(const uint8_t* in, uint64_t size, const gi::Start& s, gi::Decoder& dec, uint16_t* scratch) {
    gi::Result r = dec.run(in, size, s, nullptr, 0, scratch, (uint32_t)VERIFY_OUT, false);
    if (r.status == gi::OK) return true;
    if (r.status != gi::ERR_OVERFLOW) return false;
    return r.blocks >= VERIFY_BLOCKS || r.out_len >= VERIFY_OUT - gi::MAX_SYMBOL_OUT;
}

// A decoder of its own for each worker that checks candidates, and room for its symbols
struct Verifier {
    gi::Decoder dec;
    uint16_t luts[gi::LUT_U16];
    std::vector<uint16_t> scratch;
    explicit Verifier(size_t n = VERIFY_OUT + 2 * gi::MAX_SYMBOL_OUT) : scratch(n) { dec.attach(luts); }
};

struct Rescuer {
    const uint8_t* in;
    uint64_t size, nbits;
    uint64_t hold = HOLD;  // output bytes a pass keeps in memory
    uint64_t max_output = 0;  // a pass stops when its output grows past it (0: never)
    bool drop_alike = false;  // record a place only when no earlier one of its variable is alike (VarTable::alike)
    bool stop = false;        // the output grew past it
    gi::Decoder dec;
    uint16_t luts[gi::LUT_U16];
    std::vector<uint16_t> scratch;
    Window win;
    uint32_t next_var = 1;  // variables: the bytes of lost windows, WINDOW + 1 per window
    // The piece being decoded: its bytes from piece offset `base` on, enough for references
    // within it (WINDOW back) and for the consumer, which has taken them up to `fed`
    struct Tail {
        std::vector<uint8_t> bytes, known;
        std::vector<uint32_t> var;
        uint64_t base = 0, fed = 0, len = 0;
    } tail;
    std::vector<std::pair<uint64_t, uint64_t>> runs;  // the piece being decoded: its ranges of bytes with a variable
    uint64_t lost64 = UINT64_MAX;                      // and the first of them at a multiple of 64

    Rescuer(const uint8_t* d, uint64_t n) : in(d), size(n), nbits(n * 8), scratch(SCRATCH + 2 * gi::MAX_SYMBOL_OUT) {
        dec.attach(luts);
        if (const char* e = getenv("GR_HOLD")) hold = strtoull(e, nullptr, 10);  // for tests: 0 decodes everything again
    }

    // Does decoding from s go on cleanly for long enough to trust it?
    bool verify(const gi::Start& s) { return verify_at(in, size, s, dec, scratch.data()); }

    // Every confirmed dynamic-block header in [from, nbits). The file is scanned in chunks, in
    // parallel; within a chunk 64 positions at a time are tested for the fixed bits of a dynamic
    // header (BTYPE 2, HLIT and HDIST at most 29), then each that has them as before.
    std::vector<gi::Start> find_headers(uint64_t from) {
        constexpr uint64_t CHUNK = 1ull << 26;  // bits per task: 8 MB of input
        uint64_t end = nbits > 128 ? nbits - 128 : 0;  // positions b with b + 128 < nbits
        if (from >= end) return {};
        size_t nchunks = (size_t)((end - from + CHUNK - 1) / CHUNK);
        std::vector<std::vector<gi::Start>> found(nchunks);
        std::vector<std::unique_ptr<Verifier>> workers(threads());
        parallel_for(nchunks, [&](size_t c, unsigned w) {
            if (!workers[w]) workers[w] = std::make_unique<Verifier>();
            Verifier& vf = *workers[w];
            uint64_t a = from + c * CHUNK, e = std::min(end, a + CHUNK);
            for (uint64_t b0 = a; b0 < e; b0 += 64) {
                uint64_t lo = bits64(in, b0), hi = bits64(in, b0 + 64);
                auto at = [&](int j) { return (lo >> j) | (hi << (64 - j)); };  // bit i: input bit b0 + i + j
                uint64_t m = ~at(1) & at(2) & ~(at(4) & at(5) & at(6) & at(7)) & ~(at(9) & at(10) & at(11) & at(12));
                if (e - b0 < 64) m &= (1ull << (e - b0)) - 1;
                for (; m; m &= m - 1) {
                    uint64_t b = b0 + (uint64_t)std::countr_zero(m);
                    uint64_t v1 = bits64(in, b);
                    if (!gi::header_prefilter(v1) || !gi::header_prefilter128(v1, bits64(in, b + 64))) continue;
                    if (!gi::plausible_dynamic_header(in, size, b)) continue;
                    gi::Start s = {b, b, gi::AT_BLOCK, -1};
                    if (verify_at(in, size, s, vf.dec, vf.scratch.data())) found[c].push_back(s);
                }
            }
        });
        std::vector<gi::Start> h;
        for (auto& f : found) h.insert(h.end(), f.begin(), f.end());
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
    // of trusted output when there is one, else they are unknown. The bytes go to take(from, to,
    // false) as they are settled, all but the last DELAY: finish() hands on the rest.
    // var_base_given: the lost window's first variable, when decoding a piece again.
    template <class Take>
    gi::Result decode_piece(gi::Start s, bool stream_start, const gi::Start* targets, int ntargets, Piece& piece, Take&& take,
                            int64_t var_base_given = -1) {
        piece.in_bit_start = s.bit;
        piece.start = s;
        piece.stream_start = stream_start;
        // a piece with no window before it: its references before its start are copies of
        // the lost window's bytes, each a variable
        uint32_t var_base = 0;
        if (var_base_given >= 0) {
            var_base = (uint32_t)var_base_given;
        } else if (!win.valid && !stream_start && next_var < 0xF0000000u) {
            var_base = next_var;
            next_var += gi::WINDOW + 1;
        }
        piece.var_base = var_base;
        tail.bytes.clear();
        tail.known.clear();
        tail.var.clear();
        tail.base = tail.fed = tail.len = 0;
        runs.clear();
        lost64 = UINT64_MAX;
        const uint64_t wsize = win.valid ? win.bytes.size() : 0;
        gi::Result r;
        for (;;) {
            r = dec.run(in, size, s, targets, ntargets, scratch.data(), (uint32_t)scratch.size(), stream_start && tail.len == 0);
            uint64_t base = tail.len;
            for (uint32_t i0 = 0; i0 < r.out_len; i0 += SLICE) {
                uint32_t n = std::min<uint32_t>(SLICE, r.out_len - i0);
                size_t t = tail.bytes.size();
                tail.bytes.resize(t + n);
                tail.known.resize(t + n);
                tail.var.resize(t + n);
                uint8_t* tb = tail.bytes.data();
                uint8_t* tk = tail.known.data();
                uint32_t* tv = tail.var.data();
                const uint16_t* sc = scratch.data() + i0;
                for (uint32_t i = 0; i < n; i++) {
                    uint16_t v = sc[i];
                    size_t q = t + i;            // in the tail
                    uint64_t p = base + i0 + i;  // in the piece
                    if (v <= gi::REF) {
                        tb[q] = (uint8_t)v;
                        tk[q] = 1;
                        tv[q] = 0;
                        continue;
                    }
                    uint32_t d = v - gi::REF;
                    if (d <= p) {
                        tb[q] = tb[q - d];
                        tk[q] = tk[q - d];
                        tv[q] = tv[q - d];
                    } else if (d - p <= wsize) {
                        size_t w = wsize - (d - p);
                        tb[q] = win.bytes[w];
                        tk[q] = win.known[w];
                        tv[q] = win.var[w];
                    } else {
                        tb[q] = 0;
                        tk[q] = 0;
                        tv[q] = var_base && d - p <= gi::WINDOW ? var_base + (uint32_t)(d - p) : 0;
                    }
                }
                tail.len += n;
                if (tail.len > DELAY && tail.len - DELAY > tail.fed) {
                    take(tail.fed, tail.len - DELAY, false);
                    tail.fed = tail.len - DELAY;
                }
                trim_tail();
                if (stop) break;
            }
            if (stop) break;
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
        piece.len = tail.len;
        piece.in_bit_end = r.end_bit;
        piece.status = r.status;
        piece.landed = r.status == gi::OK && r.landed >= 0;
        return r;
    }

    // The tail keeps what references can reach and a few bytes before what the consumer has not taken
    void trim_tail() {
        uint64_t keep = std::min(tail.fed > 8 ? tail.fed - 8 : 0, tail.len > gi::WINDOW ? tail.len - gi::WINDOW : 0);
        if (keep - tail.base < TRIM) return;
        size_t cut = (size_t)(keep - tail.base);
        tail.bytes.erase(tail.bytes.begin(), tail.bytes.begin() + cut);
        tail.known.erase(tail.known.begin(), tail.known.begin() + cut);
        tail.var.erase(tail.var.begin(), tail.var.begin() + cut);
        tail.base = keep;
    }

    // The piece's end is settled: with `blank`, the bytes its last symbol may have written (read
    // from bits past the end of the file) are unknown. The rest goes to take(from, to, true).
    template <class Take>
    void finish(bool blank, Take&& take) {
        if (blank)
            for (uint64_t i = tail.len > gi::MAX_SYMBOL_OUT ? tail.len - gi::MAX_SYMBOL_OUT : 0; i < tail.len; i++) {
                tail.known[(size_t)(i - tail.base)] = 0;
                tail.var[(size_t)(i - tail.base)] = 0;
            }
        if (tail.len > tail.fed) take(tail.fed, tail.len, true);
        tail.fed = tail.len;
    }

    // A cell of the tail for an Occ: piece offset j
    uint32_t cell(uint64_t j) const {
        size_t q = (size_t)(j - tail.base);
        if (tail.known[q] == 1) return tail.bytes[q];
        return tail.var[q] ? Occ::VAR + tail.var[q] : Occ::NONE;
    }

    // A pass's consumer: piece p (decode k) has bytes [from, to) settled in the tail. They are
    // held while the pass holds fewer than `hold` bytes; whatever inference needs is noted.
    void gather(Outcome& o, Piece& p, size_t k, uint64_t from, uint64_t to) {
        size_t a = (size_t)(from - tail.base), n = (size_t)(to - from);
        const uint8_t* b = tail.bytes.data() + a;
        const uint8_t* kn = tail.known.data() + a;
        const uint32_t* v = tail.var.data() + a;
        if (p.held && o.held_bytes + n > hold) {
            // from this piece on, the bytes are decoded again to be written out
            o.unheld = k;
            o.resume = win;
            p.held = false;
            p.bytes = {};
            p.known = {};
            p.uvar = {};
        }
        if (p.held) {
            p.bytes.insert(p.bytes.end(), b, b + n);
            p.known.insert(p.known.end(), kn, kn + n);
            for (size_t i = 0; i < n; i++)
                if (!kn[i]) p.uvar.push_back(v[i]);
            o.held_bytes += n;
        }
        if (max_output && o.total + to > max_output) stop = true;
        Member& m = o.members.back();
        m.out_crc = crc32(b, n, m.out_crc);
        uint64_t exact = 0;
        Records& rec = o.rec;
        for (size_t i = 0; i < n; i++) {
            if (kn[i] == 1) {
                exact++;
                continue;
            }
            uint32_t var = v[i];
            if (!var) continue;
            uint64_t x = from + i, pos = o.total + x;
            if (!runs.empty() && runs.back().second == x) runs.back().second++;
            else runs.push_back({x, x + 1});
            if (x % 64 == 0) lost64 = std::min(lost64, x);
            uint32_t d = rec.vt.get(var);
            VarTable::Var& vr = rec.vt.vars[d];
            vr.count++;
            if (vr.nocc >= MAX_OCC) continue;
            Occ oc;
            oc.pos = pos;
            for (int q = 1; q <= Occ::BEFORE; q++) oc.cell[q - 1] = x >= (uint64_t)q ? cell(x - q) : Occ::NONE;
            for (int q = 1; q <= Occ::AFTER; q++) oc.cell[Occ::BEFORE + q - 1] = x + q < tail.len ? cell(x + q) : Occ::NONE;
            rec.vt.add_place(d, oc, drop_alike);
        }
        p.exact += exact;
        if (exact != n) p.all_exact = false;
    }

    // the trusted output grows by the piece just decoded; the window keeps its last 32 KB
    // (several pieces, when blocks are small)
    void set_window_from_tail() {
        uint64_t n = std::min<uint64_t>(tail.len, gi::WINDOW);
        size_t a = (size_t)(tail.len - n - tail.base);
        win.bytes.insert(win.bytes.end(), tail.bytes.begin() + a, tail.bytes.end());
        win.known.insert(win.known.end(), tail.known.begin() + a, tail.known.end());
        win.var.insert(win.var.end(), tail.var.begin() + a, tail.var.end());
        if (win.bytes.size() > gi::WINDOW) {
            size_t cut = win.bytes.size() - gi::WINDOW;
            win.bytes.erase(win.bytes.begin(), win.bytes.begin() + cut);
            win.known.erase(win.known.begin(), win.known.begin() + cut);
            win.var.erase(win.var.begin(), win.var.begin() + cut);
        }
        win.valid = true;
    }

    void drop_window() {
        win.bytes.clear();
        win.known.clear();
        win.var.clear();
        win.valid = false;
    }

    // Decodes sources [from, to) of a pass again, each as the pass did, from the window it had
    // before `from` (the first one not held): take(k, from, to, at_end) gets the bytes. False
    // if a decode came out other than in the pass (it cannot: decoding is a function of the input).
    template <class Take>
    bool replay(const Outcome& o, const std::vector<gi::Start>& headers, size_t from, size_t to, Take&& take) {
        win = o.resume;
        for (size_t k = from; k < to; k++) {
            const Source& s = o.sources[k];
            Piece p;
            auto t = [&](uint64_t a, uint64_t b, bool end) { take(k, a, b, end); };
            decode_piece(s.start, s.stream_start, headers.data() + s.t0, (int)(headers.size() - s.t0), p, t, s.var_base);
            if (p.len != s.len) return false;
            finish(s.blank_tail, t);
            if (s.window_after) set_window_from_tail();
            else drop_window();
        }
        return true;
    }

    // The stored block of piece p's decode whose data holds input bit `bit`: the input bytes of
    // its data, [first, second). {0, 0}: bit lies in no stored block's data. `with_len`: in its
    // length fields too (anywhere past the first bit of its header).
    std::pair<uint64_t, uint64_t> stored_around(const Piece& p, uint64_t bit, bool with_len = false) {
        if (p.start.bit == UINT64_MAX || bit <= p.start.bit) return {0, 0};
        gi::Start st = p.start;
        bool first = p.stream_start;
        for (;;) {
            // a stored block is copied whole: a stop inside its data stops right after it
            gi::Result q = dec.run(in, size, st, nullptr, 0, scratch.data(), (uint32_t)scratch.size(), first, bit);
            first = false;
            if (q.status == gi::ERR_OVERFLOW) {
                st = q.resume;
                continue;
            }
            uint64_t h = q.last_block_bit;
            if (q.status != gi::STOPPED || h == UINT64_MAX || ((bits64(in, h) >> 1) & 3) != 0) return {0, 0};
            uint64_t at = (h + 3 + 7) / 8, from = at + 4, to = from + (in[at] | in[at + 1] << 8);
            if (bit < (with_len ? h + 1 : from * 8) || bit >= to * 8 || q.end_bit != to * 8) return {0, 0};
            return {from, to};
        }
    }

    // Does a block of piece p's decode begin at bit?
    bool block_start_of(const Piece& p, uint64_t bit) {
        if (p.start.bit == UINT64_MAX || bit <= p.start.bit) return bit == p.start.bit;
        gi::Start st = p.start;
        bool first = p.stream_start;
        for (;;) {
            gi::Result q = dec.run(in, size, st, nullptr, 0, scratch.data(), (uint32_t)scratch.size(), first, bit);
            first = false;
            if (q.status == gi::ERR_OVERFLOW) {
                st = q.resume;
                continue;
            }
            return q.status == gi::STOPPED && q.resume.mode == gi::AT_BLOCK && q.resume.bit == bit;
        }
    }

    // Decoding from headers[s], landing from header to header: does the chain reach headers[j]
    // (1), end cleanly before it with a final block (0: an embedded Deflate stream, such as a
    // .gz inside a tar, whose headers are not this stream's), or break with an error (-1: damage)?
    // An embedded stream that the encoder stored lies in the data of stored blocks, and each
    // block's 5-byte header breaks it: a chain that breaks only past the end of the stored block
    // of p's decode it started in ran into the next one's header (measured, 24 false flags on an
    // undamaged Python-3.14.0.tgz). One that breaks inside it went through damage there, in
    // stored data that nothing else checks (-2). And a block of such a stream that ends where one
    // of p's stored blocks begins reads its header (a zero byte, then the length) as a stored block
    // of its own, and from there decodes as p does, landing where p lands: that says nothing of p
    // (seen in a tar holding a .gz of 127-symbol blocks). Every chain among the headers passed
    // over is followed, the next from where the one before ended.
    int chain_outcome(const std::vector<gi::Start>& headers, size_t s, size_t j, const Piece& p) {
        auto from_bit = [&](uint64_t bit) {
            bit = std::max(bit, headers[s].bit + 1);
            while (s < j && headers[s].bit < bit) s++;
        };
        while (s < j) {
            gi::Start st = headers[s];
            for (;;) {
                gi::Result q = dec.run(in, size, st, headers.data() + s + 1, (int)(j - s), scratch.data(), (uint32_t)scratch.size(), false);
                if (q.status == gi::ERR_OVERFLOW) {
                    st = q.resume;
                    continue;
                }
                if (q.status == gi::OK && q.landed >= 0) {
                    size_t began = s;
                    s = s + 1 + q.landed;
                    if (s < j) break;
                    bool merged = stored_around(p, headers[began].bit).second && q.last_block_bit != UINT64_MAX &&
                                  block_start_of(p, q.last_block_bit);
                    return merged ? 0 : 1;
                }
                if (q.status == gi::OK) {
                    from_bit(q.end_bit);
                    break;
                }
                auto [from, to] = stored_around(p, headers[s].bit);
                if (to == 0) return -1;
                if (q.end_bit + MAX_SYMBOL_BITS < to * 8) return -2;
                from_bit(std::max(q.end_bit, to * 8));
                break;
            }
        }
        return 0;
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

    // Piece t takes over p's bytes from offset at on (p keeps those before)
    static void split_bytes(Piece& p, Piece& t, uint64_t at) {
        t.src = p.src;
        t.src_off = p.src_off + at;
        t.len = p.len - at;
        t.held = p.held;
        t.exact = 0;
        t.all_exact = p.all_exact;
        if (p.held) {
            size_t u = (size_t)std::count(p.known.begin(), p.known.begin() + at, (uint8_t)0);  // unknown bytes before at
            t.bytes.assign(p.bytes.begin() + at, p.bytes.end());
            t.known.assign(p.known.begin() + at, p.known.end());
            t.uvar.assign(p.uvar.begin() + u, p.uvar.end());
            p.bytes.resize(at);
            p.known.resize(at);
            p.uvar.resize(u);
        }
        p.len = at;
        p.exact = 0;
    }

    // A block holds no more symbols than the stream's regular count: zlib fills each to it but
    // the last. A decode that ran off the end of the file through more than that went through
    // damage that decodes as plausible data (a stretch of zeros, say): from that block on, the
    // bytes are suspect. Without this they would pass for a truncated file's last exact bytes.
    void suspect_overfull_tail(Outcome& o, uint32_t regular) {
        if (!regular || !o.truncated || o.pieces.empty()) return;
        Piece& p = o.pieces.back();
        if (p.suspect || p.status != gi::ERR_INPUT || p.last_block_bit == UINT64_MAX || p.last_block_at > p.len) return;
        uint64_t n = block_symbols(p.last_block_bit);
        if (n <= regular) return;
        Piece t;
        split_bytes(p, t, p.last_block_at);
        t.in_bit_start = p.last_block_bit;
        t.start = {p.last_block_bit, p.last_block_bit, gi::AT_BLOCK, -1};
        t.anchored = p.anchored;  // the decode reached its header in step from p's start
        t.in_bit_end = p.in_bit_end;
        t.status = p.status;
        t.blocks = 1;
        t.symbols = (uint32_t)std::min<uint64_t>(n, UINT32_MAX);
        t.suspect = true;
        t.why = "its block holds " + std::to_string(n) + " symbols, more than the stream's regular " + std::to_string(regular) +
                ": the decode ran through damaged data to the end of the file";
        p.in_bit_end = p.last_block_bit;
        p.status = gi::OK;
        p.blocks = p.blocks ? p.blocks - 1 : 0;
        p.symbols = p.symbols > t.symbols ? p.symbols - t.symbols : 0;
        if (p.len == 0) {
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

    // The first run of at least RUN bytes of 0x00 (zeroed storage, a crash, a hole in a download)
    // or 0xFF (erased flash) that starts in [from, to). UINT64_MAX: none.
    uint64_t first_run(uint64_t from, uint64_t to, uint64_t& len, uint8_t& value) const {
        to = std::min(to, size);
        for (uint64_t i = from; i < to;) {
            uint8_t v = in[i];
            uint64_t j = i + 1;
            if (v == 0 || v == 0xff) {
                while (j < size && in[j] == v) j++;
                if (j - i >= RUN) {
                    len = j - i;
                    value = v;
                    return i;
                }
            }
            i = j;
        }
        return UINT64_MAX;
    }

    // What piece p's decode produced from input before bit `damage`, every bit of every symbol
    // read from before it: {output bytes, symbols}. Decoding is a function of the bits read, so
    // these are what the undamaged file decodes to, whatever lies from `damage` on.
    std::pair<uint64_t, uint64_t> output_before(const Piece& p, uint64_t damage) {
        if (p.start.bit == UINT64_MAX || damage < p.start.bit + MAX_SYMBOL_BITS) return {0, 0};
        uint64_t stop = damage - MAX_SYMBOL_BITS;  // a symbol begun before it ends before the damage
        gi::Start st = p.start;
        bool first = p.stream_start;
        uint64_t out = 0, syms = 0;
        for (;;) {
            gi::Result q = dec.run(in, size, st, nullptr, 0, scratch.data(), (uint32_t)scratch.size(), first, stop);
            first = false;
            if (q.status == gi::ERR_OVERFLOW) {
                out += q.out_len;
                syms += q.symbols;
                st = q.resume;
                continue;
            }
            if (q.status != gi::STOPPED) return {0, 0};  // p's own decode went on past here: not reached
            // a stored block is copied whole: one begun before the stop may reach past the damage
            if (q.end_bit > damage && q.last_block_bit != UINT64_MAX && q.out_len > q.last_block_out)
                return {out + q.last_block_out, syms + q.symbols};
            return {out + q.out_len, syms + q.symbols};
        }
    }

    // A member decoded as one piece whose CRC or length fails though it decoded in step was
    // damaged in a way decoding cannot see: zeroed storage decodes as valid codes, and the
    // decoder falls back in step after it. The piece is flagged, from a run of zeros (or 0xFF
    // bytes) in its data on when there is one (localize splits it there), else whole. Only the
    // bytes before the run are vouched for. A member of several pieces is left alone: its CRC
    // says something is wrong somewhere, and blaming the pieces with zeros would vouch for the
    // others, which nothing checks (measured: linux-6.6.tar.gz with three zeroed sectors, each
    // decoded in step; the first flagged, 53 wrong files were called intact). Returns the pieces
    // to flag, by start bit, and why.
    std::map<uint64_t, std::string> crc_misses(const Outcome& o, std::vector<std::string>& notes) {
        std::map<uint64_t, std::string> out;
        for (const Member& m : o.members) {
            if (!m.trailer || m.first_piece != m.last_piece || m.last_piece >= o.pieces.size() ||
                o.pieces[m.last_piece].status != gi::OK)
                continue;
            const Piece& p = o.pieces[m.last_piece];
            if (p.suspect || !p.all_exact) continue;
            uint32_t c = m.out_crc;  // the member is this one piece
            if (!((m.has_crc && c != m.crc) || (m.size_known && (uint32_t)p.len != m.isize))) continue;
            std::string what = std::string("the stream's ") + (m.has_crc ? "CRC" : "length") + " does not match what was decoded";
            uint64_t n = 0;
            uint8_t v = 0;
            uint64_t at = first_run(p.in_bit_start / 8, (p.in_bit_end + 7) / 8, n, v);
            if (at != UINT64_MAX) {
                std::string run = "a run of " + std::to_string(n) + (v ? " 0xFF" : " zero") + " bytes at input byte " + std::to_string(at);
                out[p.in_bit_start] = what + ", and " + run + " lies in this block: damage that decodes as valid codes";
                notes.push_back(what + ", though it decoded in step: the damage is placed where the " + (v ? "0xFF bytes" : "zeros") +
                                " begin, and the output from there on is flagged. The bytes before them decoded cleanly; with the "
                                "CRC failing, nothing more confirms them");
            } else {
                out[p.in_bit_start] = what + ", and nothing shows where in this, the stream's only piece, the damage lies";
                notes.push_back(what + ", though it decoded in step, and nothing shows where the damage lies: decoded in one piece (" +
                                std::to_string(p.blocks) + " block" + (p.blocks == 1 ? "" : "s") + "), all of it is flagged");
            }
        }
        return out;
    }

    // Bit rot: a flipped bit makes the decoder read a wrong code, and it breaks soon after (or
    // falls back in step: the block's count shows it, or nothing does, a flipped literal). Does
    // one flipped bit in the FLIP_BITS before where piece p broke mend its block, landing on the
    // next header with a count that fits the stream's habit? Then the damage is bit rot, which
    // elsewhere may have changed nothing a decoder sees. Damage of many bytes (other data written
    // over a cluster, a gap, a stretch twice) is not mended by one bit. buf: the input, which
    // each trial flips a bit of and puts back.
    bool one_flip_mends(const Piece& p, const std::vector<gi::Start>& headers, uint8_t* buf, const Habit& habit) {
        if (p.start.mode != gi::AT_BLOCK || p.start.bit == UINT64_MAX || p.status != gi::ERR_DATA) return false;
        auto t = std::upper_bound(headers.begin(), headers.end(), p.start.bit, [](uint64_t b, const gi::Start& s) { return b < s.bit; });
        if (t == headers.end()) return false;
        const gi::Start* targets = &*t;
        int ntargets = (int)(headers.end() - t);
        uint64_t e = std::min(p.in_bit_end, nbits), lo = e > p.start.bit + FLIP_BITS ? e - FLIP_BITS : p.start.bit;
        // where a trial decodes from: the block's start, and symbol boundaries about every 64 bits
        // from lo on, with the symbols and blocks before each (a flip after a boundary leaves what
        // came before it as it was; one in a block's header is read again from any boundary in a
        // block before it)
        struct Snap {
            gi::Start at;
            uint32_t symbols, blocks;
        };
        std::vector<Snap> snaps{{p.start, 0, 0}};
        gi::Start st = p.start;
        uint32_t syms = 0, blocks = 0;
        bool first = p.stream_start;
        for (uint64_t stop = lo; stop < e;) {
            gi::Result q = dec.run(in, size, st, nullptr, 0, scratch.data(), (uint32_t)scratch.size(), first, stop);
            first = false;
            syms += q.symbols;
            blocks += q.blocks;
            if (q.status == gi::ERR_OVERFLOW) {
                st = q.resume;
                continue;
            }
            if (q.status != gi::STOPPED) break;
            if (q.resume.bit > snaps.back().at.bit) snaps.push_back({q.resume, syms, blocks});
            st = q.resume;
            stop = q.resume.bit + 64;
        }
        for (uint64_t f = e; f-- > lo;) {
            size_t k = snaps.size() - 1;
            while (k > 0 && snaps[k].at.bit > f) k--;
            buf[f >> 3] ^= (uint8_t)(1u << (f & 7));
            gi::Start s2 = snaps[k].at;
            uint32_t sy = snaps[k].symbols, bl = snaps[k].blocks;
            bool from_start = k == 0 && p.stream_start, mended = false;
            for (;;) {
                gi::Result q = dec.run(in, size, s2, targets, ntargets, scratch.data(), (uint32_t)scratch.size(), from_start);
                from_start = false;
                sy += q.symbols;
                bl += q.blocks;
                if (q.status == gi::ERR_OVERFLOW) {
                    s2 = q.resume;
                    continue;
                }
                mended = q.status == gi::OK && q.landed == 0 && habit.fits(sy, bl);
                break;
            }
            buf[f >> 3] ^= (uint8_t)(1u << (f & 7));
            if (mended) return true;
        }
        return false;
    }

    // Damage that writes zeros (or erases to 0xFF) begins where the run of them begins. A damaged
    // piece that decoded cleanly from a start in step up to such a run, and broke only within it
    // or after it, holds exact bytes up to it and suspect ones from it on: it is split there. A
    // truncated decode that ran through such a run is suspect from it on.
    void localize(Outcome& o, const std::vector<gi::Start>& headers) {
        std::vector<Piece> out;
        std::vector<size_t> head_at(o.pieces.size()), tail_at(o.pieces.size());
        for (size_t k = 0; k < o.pieces.size(); k++) {
            Piece& p = o.pieces[k];
            head_at[k] = out.size();
            bool runs_out = !p.suspect && o.truncated && k + 1 == o.pieces.size() && p.status == gi::ERR_INPUT;
            uint64_t n = 0, at = UINT64_MAX;
            uint8_t v = 0;
            if (p.suspect && p.anchored && p.start.mode == gi::AT_BLOCK) {
                // the damage lies before where the decode broke (its last symbol read up to
                // MAX_SYMBOL_BITS past that) or landed, and before any header it passed over:
                // a decode in step lands on every real header
                uint64_t limit = p.in_bit_end + (p.status == gi::OK ? 0 : MAX_SYMBOL_BITS);
                // (a header in the data of one of its stored blocks is an embedded stream's)
                auto h = std::upper_bound(headers.begin(), headers.end(), p.in_bit_start,
                                          [](uint64_t b, const gi::Start& s) { return b < s.bit; });
                while (h != headers.end() && h->bit < p.in_bit_end && stored_around(p, h->bit).second) h++;
                if (h != headers.end() && h->bit < p.in_bit_end) limit = std::min(limit, h->bit);
                at = first_run(p.in_bit_start / 8, (limit + 7) / 8, n, v);
            } else if (runs_out) {
                at = first_run(p.in_bit_start / 8, size, n, v);
            }
            auto [nb, ns] = at == UINT64_MAX ? std::pair<uint64_t, uint64_t>{0, 0} : output_before(p, at * 8);
            nb = std::min<uint64_t>(nb, p.len);
            if (at == UINT64_MAX || (nb == 0 && !runs_out)) {
                out.push_back(std::move(p));
                tail_at[k] = out.size() - 1;
                continue;
            }
            std::string run = "a run of " + std::to_string(n) + (v ? " 0xFF" : " zero") + " bytes at input byte " + std::to_string(at);
            Piece t;
            split_bytes(p, t, nb);
            t.in_bit_start = at * 8;
            t.in_bit_end = p.in_bit_end;
            t.suspect = true;
            t.by_crc = p.by_crc;
            t.length_exact = p.length_exact;
            t.status = p.status;
            t.landed = p.landed;
            t.blocks = 1;
            t.symbols = (uint32_t)(p.symbols > ns ? p.symbols - ns : 0);
            if (runs_out)
                t.why = "the decode ran through " + run + " and on to the end of the file: zeros where compressed data should be "
                        "decode as valid codes, so nothing from them on is trusted";
            else if (p.by_crc)
                t.why = p.why;
            else
                t.why = p.why + "; the damage begins at " + run + ": the " + std::to_string(nb) + " bytes decoded before it are exact";
            if (nb > 0) {
                p.in_bit_end = at * 8;
                p.suspect = p.by_crc = p.landed = false;
                p.why.clear();
                p.status = gi::STOPPED;
                p.symbols = (uint32_t)ns;
                out.push_back(std::move(p));
            } else {
                t.stream_start = p.stream_start;
            }
            out.push_back(std::move(t));
            tail_at[k] = out.size() - 1;
            if (runs_out)
                for (std::string& note : o.notes)
                    if (note == "the file ends inside the stream (truncated)")
                        note = "the decode ran through " + run + " to the end of the file: from there on the output is flagged";
        }
        for (Member& m : o.members) {
            if (m.first_piece < head_at.size()) m.first_piece = head_at[m.first_piece];
            if (m.last_piece < tail_at.size()) m.last_piece = tail_at[m.last_piece];
        }
        o.pieces.swap(out);
    }

    // One pass over the file. `forced`: start bits of pieces known to be damaged from an earlier
    // pass, and why. gzip: the stream starts after a member header of hlen bytes (0: damaged,
    // start at the first block header found) and ends with a trailer, more members may follow.
    // Raw: the stream starts at bit 0; the caller knows its size and CRC, if anyone does.
    Outcome rescue(const std::vector<gi::Start>& headers, bool gzip, uint64_t hlen, const std::map<uint64_t, std::string>& forced,
                   uint64_t expected_size = UINT64_MAX, bool has_crc = false, uint32_t expected_crc = 0) {
        Outcome o;
        drop_window();
        next_var = 1;
        stop = false;
        bool stream_start = !gzip || hlen != 0;
        bool in_step = stream_start;  // the decode before the next piece landed on its start
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
        std::vector<std::pair<uint64_t, uint64_t>> last_runs;  // those of the last piece that was not damaged
        size_t last_k = SIZE_MAX;
        // the piece decoded is settled: its last bytes go to gather, it joins the outcome, and
        // the window after it holds its last bytes or nothing
        auto commit = [&](Piece& p, size_t k, bool blank, bool window_after) {
            finish(blank, [&](uint64_t a, uint64_t b, bool) { gather(o, p, k, a, b); });
            Source& s = o.sources[k];
            s.len = p.len;
            s.blank_tail = blank;
            s.window_after = window_after;
            s.lost64 = lost64;
            // the ranges with variables of a piece that may be split: a damaged one, or the last
            if (p.suspect) o.rec.runs[k] = std::move(runs);
            else last_runs = std::move(runs);
            last_k = k;
            if (p.held) {
                p.bytes.shrink_to_fit();
                p.known.shrink_to_fit();
                p.uvar.shrink_to_fit();
            }
            o.total += p.len;
            o.pieces.push_back(std::move(p));
            if (window_after) set_window_from_tail();
            else drop_window();
        };
        size_t t0 = 0;  // headers[t0] is the first header after cur
        while (cur.bit < nbits) {
            while (t0 < headers.size() && headers[t0].bit <= cur.bit) t0++;
            size_t k = o.sources.size();
            o.sources.push_back({cur, stream_start, t0, 0, o.total, 0, false, false});
            Piece p;
            p.held = o.unheld == SIZE_MAX;
            p.src = k;
            gi::Result r = decode_piece(cur, stream_start, headers.data() + t0, (int)(headers.size() - t0), p,
                                        [&](uint64_t a, uint64_t b, bool) { gather(o, p, k, a, b); });
            o.sources[k].var_base = p.var_base;
            if (stop) {
                o.too_big = true;
                break;
            }
            stream_start = false;
            p.anchored = in_step || (r.status == gi::OK && r.landed == 0);
            in_step = false;
            Member& m = o.members.back();
            m.last_piece = o.pieces.size();
            auto fb = forced.find(p.in_bit_start);
            bool forced_bad = fb != forced.end();
            if (forced_bad) {
                p.suspect = true;
                p.why = fb->second;
            }
            uint64_t t = (r.end_bit + 7) / 8, next = gzip ? t + 8 : t;
            uint64_t nh = gzip && next < size ? gzip_header(in, size, next) : 0;
            // a final block is believed when the file ends after its trailer, another member
            // follows, or no block header is left: garbage decodes end in fake final blocks.
            // A raw stream is also believed when its output has the size the caller expects.
            bool real_end = next >= size || nh || t0 >= headers.size() ||
                            (!gzip && expected_size != UINT64_MAX && o.total + p.len == expected_size);
            if (r.status == gi::OK && r.landed == 0 && !forced_bad) {  // landed on the next header: in step
                commit(p, k, false, true);
                cur = headers[t0];
                in_step = true;
                continue;
            }
            if (r.status == gi::OK && r.landed > 0 && !forced_bad) {
                // passed over headers: real ones, missed because the decode went through damage,
                // or an embedded stream's, which this stream never lands on
                size_t j = t0 + r.landed;
                int chain = chain_outcome(headers, t0, j, p);
                // Headers that all lie in stored blocks this decode went through are not this
                // stream's: a block header never sits inside a stored block's bytes, and the
                // decode, having copied them, landed on a header of its own. However the chain
                // from them goes, they are the data's (measured: a tar holding a .gz of 127-symbol
                // blocks whose chain went on to land where p lands, as if p had missed them; the
                // same tar with other file contents did not, so it hung on where blocks fell).
                // Their length fields count too: the search can find a header that begins there
                // and reads on into the data (measured: one such in a stored block's NLEN, at 1 of
                // 120 places of that .gz, with zlib-ng; the decode then restarted inside the .gz,
                // and its 127-symbol blocks, taken for this stream's habit, flagged 18 real ones).
                if (chain == 1) {
                    bool all_stored = true;
                    for (size_t q = t0; q < j && all_stored; q++) all_stored = stored_around(p, headers[q].bit, true).second != 0;
                    if (all_stored) chain = 0;
                }
                if (chain == 0) {
                    commit(p, k, false, true);
                    cur = headers[j];
                    t0 = j;
                    in_step = true;
                    continue;
                }
                if (chain == -2) {
                    // the decode itself stayed in step: it goes on from where it landed
                    p.suspect = p.length_exact = true;
                    p.why = "a Deflate stream embedded in its stored data breaks inside it: the stored bytes are damaged";
                    commit(p, k, false, false);
                    cur = headers[j];
                    t0 = j;
                    in_step = true;
                    continue;
                }
                p.why = "its decode passed over the next block header without landing on it";
            }
            if (r.status == gi::OK && r.landed < 0 && real_end) {  // the member's last block (flagged, when forced)
                // a forced one leaves no window; nor does the start of another member
                commit(p, k, false, !forced_bad && !nh);
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
                    stream_start = in_step = true;
                    continue;
                }
                if (next < size) o.notes.push_back(std::to_string(size - next) + " bytes after the last member were not gzip data");
                break;
            }
            if (r.status == gi::ERR_INPUT && t0 >= headers.size()) {
                // the last symbol may have been read from bits past the end: what one symbol can
                // write (258 bytes) is not trusted
                commit(p, k, true, false);
                o.notes.push_back("the file ends inside the stream (truncated)");
                o.truncated = true;
                break;
            }
            if (p.why.empty()) {
                p.why = r.status == gi::OK && r.landed < 0 ? "a final block was read where the data goes on"
                        : r.status == gi::ERR_INPUT      ? "the decode ran off the end of the file"
                                                         : "the decode hit invalid data";
            }
            // damaged: keep what was decoded, restart at the next header, trust no window. A forced
            // piece that landed past the next header passed over an embedded stream's (an earlier
            // pass found it in step): it restarts where it landed
            p.suspect = true;
            commit(p, k, false, false);
            if (forced_bad && r.status == gi::OK && r.landed > 0) t0 += r.landed;
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
        if (last_k != SIZE_MAX && !o.rec.runs.count(last_k)) o.rec.runs[last_k] = std::move(last_runs);
        return o;
    }
};

// A member's CRC and length over what a pass decoded: 1 right, 0 wrong, -1 not checkable (no
// trailer, a damaged piece, unknown bytes)
int member_crc(const Outcome& o, const Member& m) {
    if (!m.trailer || m.last_piece >= o.pieces.size() || o.pieces[m.last_piece].status != gi::OK) return -1;
    uint64_t len = 0;
    for (size_t k = m.first_piece; k <= m.last_piece; k++) {
        const Piece& p = o.pieces[k];
        if (p.suspect || !p.all_exact) return -1;
        len += p.len;
    }
    bool size_ok = !m.size_known || (uint32_t)len == m.isize;
    if (m.has_crc) return m.out_crc == m.crc && size_ok;
    return m.size_known && !size_ok ? 0 : -1;
}

// Encoders end their blocks by habit. zlib fills every block to 2^k - 1 symbols (16,383 by
// default); pigz, which compresses its input in chunks with zlib (128 KiB by default), also ends
// one at the end of each chunk, most often followed by empty blocks that mark it (measured:
// linux-6.6.tar.gz from kernel.org, where 1,373 of 3,976 blocks of one piece end a chunk). GNU
// gzip fills its blocks to 32,767 but may end one early at any multiple of 4,096 symbols, where
// it checks whether that compresses better (trees.c, ct_tally): in a .tar.gz from python.org a
// quarter of the blocks do, and counting them as damaged flagged 175 blocks of an undamaged
// Python-3.14.0.tgz. Pieces of one block whose count does not fit the stream's habit are damaged
// in a way landing cannot see. A known encoder's habit is believed when half the blocks fit it,
// a quarter in a file that went through a CR LF to LF transfer (two blocks in five lost a CR
// there: 280 of 687 in Python-3.14.0.tgz); any other count when 70% of the blocks have it.
// `whole`: pieces of members whose CRC checked on this pass, not flagged (their counts are the
// encoder's). Returns the start bits of the pieces to flag; habit: the one the stream has (NONE
// when no habit shows).
std::set<uint64_t> symbol_count_anomalies(const Outcome& o, const std::vector<char>& whole, bool cr_stripped, std::string& note,
                                          Habit& habit) {
    habit = Habit{};
    size_t n = o.pieces.size();
    std::vector<char> last(n, 0), chunk_end(n, 0);
    std::vector<uint64_t> end(n, 0);  // where each piece's output ends, in its member's
    for (const Member& m : o.members) {
        if (m.last_piece < n) last[m.last_piece] = 1;
        uint64_t at = 0;
        for (size_t k = m.first_piece; k <= m.last_piece && k < n; k++) end[k] = at += o.pieces[k].len;
    }
    auto eligible = [&](size_t k) {
        const Piece& p = o.pieces[k];
        return p.blocks == 1 && p.landed && !last[k] && !p.suspect;
    };
    std::map<uint32_t, int> freq;
    int considered = 0;
    for (size_t k = 0; k < n; k++)
        if (eligible(k)) {
            freq[o.pieces[k].symbols]++;
            considered++;
        }
    std::set<uint64_t> out;
    if (considered < 10) return out;
    // pigz's chunk: the commonest distance in output between the marked chunk ends (pieces of
    // several blocks), when it is a multiple of 1 KiB that half the distances are. A piece of one
    // block that ends a whole number of chunks after one, with nothing damaged between, ends one.
    std::map<uint64_t, int> gaps;
    int ngaps = 0;
    for (const Member& m : o.members) {
        uint64_t prev = 0;
        for (size_t k = m.first_piece; k <= m.last_piece && k < n; k++) {
            if (o.pieces[k].suspect) prev = UINT64_MAX;
            if (o.pieces[k].suspect || o.pieces[k].blocks < 2 || last[k]) continue;
            if (prev != UINT64_MAX) {
                gaps[end[k] - prev]++;
                ngaps++;
            }
            prev = end[k];
        }
    }
    uint64_t chunk = 0;
    if (!gaps.empty()) {
        auto g = std::max_element(gaps.begin(), gaps.end(), [](auto& a, auto& b) { return a.second < b.second; });
        if (g->first >= 32768 && g->first % 1024 == 0 && 2 * g->second >= ngaps) chunk = g->first;
    }
    if (chunk)
        for (const Member& m : o.members) {
            uint64_t base = 0;
            for (size_t k = m.first_piece; k <= m.last_piece && k < n; k++) {
                const Piece& p = o.pieces[k];
                if (p.suspect) base = UINT64_MAX;
                else if (base != UINT64_MAX && (end[k] - base) % chunk == 0) chunk_end[k] = 1;
                if (!p.suspect && p.blocks >= 2) base = end[k];
            }
        }
    // the habits: GNU gzip's; zlib's 2^k - 1, with pigz's chunk ends; one count for most blocks
    auto gnu = [](uint32_t s) { return s == 32767 || (s < 32768 && s % 4096 == 0); };
    int gnu_fit = 0, zlib_fit = 0;
    for (auto& [s, c] : freq) gnu_fit += gnu(s) ? c : 0;
    uint32_t zlib = 0;
    for (int b = 7; b <= 15; b++) {
        auto f = freq.find((1u << b) - 1);
        if (f != freq.end() && f->second > zlib_fit) {
            zlib = f->first;
            zlib_fit = f->second;
        }
    }
    int ends = 0;
    for (size_t k = 0; k < n; k++) ends += eligible(k) && o.pieces[k].symbols != zlib && chunk_end[k];
    if (zlib) zlib_fit += ends;
    auto best = std::max_element(freq.begin(), freq.end(), [](auto& a, auto& b) { return a.second < b.second; });
    int need = cr_stripped ? (considered + 3) / 4 : (considered + 1) / 2;
    using H = Habit;
    if (freq.count(32767) && gnu_fit >= need && gnu_fit >= zlib_fit) habit = {H::GNU, 32767};
    else if (zlib && zlib_fit >= need) habit = {H::ZLIB, zlib};
    else if (best->second >= considered * 7 / 10) habit = {H::ONE, best->first};
    if (habit.kind == H::NONE) return out;
    uint32_t regular = habit.regular;
    int fit = habit.kind == H::GNU ? gnu_fit : habit.kind == H::ZLIB ? zlib_fit : best->second;
    auto fits = [&](size_t k) { return habit.fits(o.pieces[k].symbols, 1) || (habit.kind == H::ZLIB && chunk_end[k]); };
    for (size_t k = 0; k < n; k++)
        if (eligible(k) && !whole[k] && !fits(k)) out.insert(o.pieces[k].in_bit_start);
    if (!out.empty())
        note = std::to_string(out.size()) + " block(s) with a symbol count " +
               (habit.kind == H::GNU ? std::string("GNU gzip never writes (it ends a block at 32767 symbols or a multiple of 4096: ")
                                     : "other than the stream's regular " + std::to_string(regular) +
                                           (habit.kind == H::ZLIB && chunk ? " that do not end one of pigz's " + std::to_string(chunk >> 10) + " KiB chunks"
                                                                           : std::string()) +
                                           " (") +
               std::to_string(fit) + " of " + std::to_string(considered) + " blocks fit that)";
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

    // learn() for a piece that is not held: its exact bytes, as runs at their offsets in its
    // decode, from offset off for len bytes. Bytes not exact lie between runs.
    void learn_runs(const Learned& runs, uint64_t off, uint64_t len, uint64_t& budget) {
        uint64_t ctx = 0, prev = off;
        int run = 0;
        for (const auto& [at, x] : runs) {
            if (!budget) break;
            uint64_t a = std::max(at, off), e = std::min<uint64_t>(at + x.size(), off + len);
            if (a >= e) continue;
            if (a != prev) {
                run = 0;
                ctx = 0;
            }
            for (uint64_t i = a; i < e && budget; i++) {
                uint8_t c = x[(size_t)(i - at)];
                uni[c]++;
                n++;
                budget--;
                for (int k = 1; k <= ORDERS && k <= run; k++) {
                    add(key(ctx, k, c));
                    add(key(ctx, k, -1));
                }
                ctx = ctx << 8 | c;
                run++;
            }
            prev = e;
        }
    }

    // The pieces in the order the model learns from them: nearest a piece with lost bytes first
    static std::vector<size_t> order(const std::vector<uint8_t>& lost) {
        size_t np = lost.size();
        std::vector<int64_t> dist(np, INT64_MAX);
        // the distance to the nearest such piece: one sweep from each side
        for (int64_t j = 0, last = -1; j < (int64_t)np; j++) {
            if (lost[j]) last = j;
            if (last >= 0) dist[j] = j - last;
        }
        for (int64_t j = (int64_t)np - 1, next = -1; j >= 0; j--) {
            if (lost[j]) next = j;
            if (next >= 0) dist[j] = std::min<int64_t>(dist[j], next - j);
        }
        std::vector<size_t> ord(np);
        for (size_t k = 0; k < np; k++) ord[k] = k;
        std::stable_sort(ord.begin(), ord.end(), [&](size_t a, size_t b) { return dist[a] < dist[b]; });
        return ord;
    }

    // Learns from TRAIN exact bytes of the pieces in `ord`, but suspect ones: learn(k, budget)
    // learns piece k
    template <class Learn>
    void build(const std::vector<Piece>& pieces, const std::vector<size_t>& ord, Learn&& learn_piece) {
        keys.assign(1u << BITS, 0);
        vals.assign(1u << BITS, 0);
        uni.assign(256, 0);
        uint64_t budget = TRAIN;
        for (size_t k : ord) {
            if (!budget) break;
            if (!pieces[k].suspect) learn_piece(k, budget);
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

/// Infers the bytes of lost windows from what the pass gathered: each variable's places and the
// bytes around them, and the exact bytes of the pieces nearest the damage, which a model learns
// from (decoded again for the pieces not held). Returns the values found, `done` the output
// bytes they give a value to; empty when nothing can be inferred.
Values infer_window(Outcome& o, Rescuer& rs, const std::vector<gi::Start>& headers, uint64_t& done, std::string& note) {
    constexpr int AHEAD = Occ::AFTER;  // bytes after the candidate that are scored with it
    done = 0;
    Values out;
    Records& rec = o.rec;
    VarTable& vt = rec.vt;
    const uint32_t nv = (uint32_t)vt.vars.size();
    if (!nv) return out;
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
    std::vector<Piece>& pieces = o.pieces;
    size_t np = pieces.size();
    std::vector<uint64_t> start(np);  // where each piece's output begins
    for (size_t k = 0; k < np; k++) start[k] = o.sources[pieces[k].src].out + pieces[k].src_off;
    // a piece has lost bytes when one at an offset that is a multiple of 64 copies a variable
    std::vector<uint8_t> lost(np, 0);
    for (size_t k = 0; k < np; k++) {
        const Piece& p = pieces[k];
        const Source& s = o.sources[p.src];
        if (p.src_off == 0) {
            lost[k] = s.lost64 < p.len;  // the decode's own offsets
            continue;
        }
        auto it = rec.runs.find(p.src);  // the part of a decode split off: its own offsets differ
        if (it == rec.runs.end()) throw std::runtime_error("the ranges with variables of a split decode were not kept");
        uint64_t a = p.src_off, e = a + p.len;
        for (const auto& [r0, r1] : it->second) {
            if (r1 <= a || r0 >= e) continue;
            uint64_t x = a + (std::max(r0, a) - a + 63) / 64 * 64;  // the first multiple of 64 from the part's start
            if (x < std::min(r1, e)) {
                lost[k] = 1;
                break;
            }
        }
    }
    // the cells of each place that lie outside its piece (since split) are not known (in
    // parallel, a block of variables at a time: each place is one variable's)
    constexpr size_t VARS_PER_TASK = 4096;
    parallel_for((nv + VARS_PER_TASK - 1) / VARS_PER_TASK, [&](size_t b, unsigned) {
        for (uint32_t d = (uint32_t)(b * VARS_PER_TASK); d < nv && d < (b + 1) * VARS_PER_TASK; d++) {
            uint32_t v = vt.vars[d].var;
            vt.each_place(d, [&](uint64_t r, Occ& oc) {
                uint64_t pos = oc.pos;
                size_t k = (size_t)(std::upper_bound(start.begin(), start.end(), pos) - start.begin()) - 1;
                uint64_t a = start[k], e = a + pieces[k].len;
                bool cut = false;
                for (int q = 1; q <= Occ::BEFORE; q++)
                    if (pos < a + q && oc.cell[q - 1] != Occ::NONE) {
                        oc.cell[q - 1] = Occ::NONE;
                        cut = true;
                    }
                for (int q = 1; q <= Occ::AFTER; q++)
                    if (pos + q >= e && oc.cell[Occ::BEFORE + q - 1] != Occ::NONE) {
                        oc.cell[Occ::BEFORE + q - 1] = Occ::NONE;
                        cut = true;
                    }
                if (cut) vt.occs.rewrite(r, oc, v);
            });
        }
    });
    uint64_t unknown = 0;
    for (const VarTable::Var& x : vt.vars) unknown += x.count;
    // The model learns from the pieces nearest the damage. Those not held are decoded again to
    // learn from them: the ones the walk below may reach (it counts only what it is sure of).
    std::vector<size_t> ord = Model::order(lost);
    std::vector<uint8_t> need(o.sources.size(), 0);
    size_t last_need = SIZE_MAX;
    uint64_t sure = TRAIN;
    for (size_t k : ord) {
        if (!sure) break;
        const Piece& p = pieces[k];
        if (p.suspect) continue;
        if (!p.held && !rec.learned.count(p.src)) {
            need[p.src] = 1;
            last_need = last_need == SIZE_MAX ? p.src : std::max(last_need, p.src);
        }
        sure -= std::min(sure, p.exact);
    }
    if (last_need != SIZE_MAX) {
        for (size_t k = 0; k < need.size(); k++)
            if (need[k]) rec.learned[k];  // an empty piece has nothing to learn, but is there
        std::vector<uint64_t> got(o.sources.size(), 0);
        bool same = rs.replay(o, headers, o.unheld, last_need + 1, [&](size_t k, uint64_t a, uint64_t b, bool) {
            if (!need[k]) return;
            Learned& runs = rec.learned[k];
            const Rescuer::Tail& t = rs.tail;
            for (uint64_t i = a; i < b && got[k] < TRAIN; i++) {
                size_t q = (size_t)(i - t.base);
                if (t.known[q] != 1) continue;
                if (runs.empty() || runs.back().first + runs.back().second.size() != i) runs.push_back({i, {}});
                runs.back().second.push_back(t.bytes[q]);
                got[k]++;
            }
        });
        if (!same) throw std::runtime_error("a decode came out differently the second time");
    }
    Model m;
    m.build(pieces, ord, [&](size_t k, uint64_t& budget) {
        const Piece& p = pieces[k];
        if (p.held) m.learn(p, budget);
        else {
            auto it = rec.learned.find(p.src);
            if (it == rec.learned.end()) throw std::runtime_error("a piece the model learns from was not decoded again");
            m.learn_runs(it->second, p.src_off, p.len, budget);
        }
    });
    if (m.empty) return out;
    FILE* dbg = getenv("GR_INFER_DEBUG") ? fopen(getenv("GR_INFER_DEBUG"), "w") : nullptr;
    out.vt = &vt;
    out.value.assign(nv, -1);
    // a cell as a context byte for variable `self`; -1: not known (itself included)
    auto val = [&](uint32_t c, uint32_t self) -> int {
        if (c < Occ::VAR) return (int)c;
        if (c == Occ::NONE || c - Occ::VAR == self) return -1;
        uint32_t d = vt.find(c - Occ::VAR);
        return d == UINT32_MAX ? -1 : out.value[d];
    };
    // the variables, by index, in the order of their numbers
    std::vector<uint32_t> todo(nv);
    for (uint32_t d = 0; d < nv; d++) todo[d] = d;
    std::sort(todo.begin(), todo.end(), [&](uint32_t a, uint32_t b) { return vt.vars[a].var < vt.vars[b].var; });
    // A variable is scored again only when one around it was resolved since: otherwise its
    // contexts, and so its scores, are what they were (scoring one again that did not change gives
    // what it gave). Most variables around a place are those of the next window bytes, copied
    // along with it: q bytes before variable v lies v + q, q after v - q. Those are scored again
    // whenever one so near is resolved; the others u lies around are listed, in
    // wlist[wstart[u] .. wstart[u + 1]).
    auto nearby = [&](uint32_t d, std::vector<uint32_t>& near) {
        near.clear();
        uint32_t v = vt.vars[d].var;
        vt.each_place(d, [&](uint64_t, const Occ& oc) {
            for (int j = 0; j < Occ::CELLS; j++) {
                uint32_t c = oc.cell[j];
                if (c == Occ::NONE || c < Occ::VAR || c - Occ::VAR == v) continue;
                uint32_t q = j < Occ::BEFORE ? (uint32_t)j + 1 : (uint32_t)(Occ::BEFORE - 1 - j);  // v + q, or v - q after
                if (c - Occ::VAR == v + q) continue;  // a near one, scored again without a list
                uint32_t u = vt.find(c - Occ::VAR);
                if (u != UINT32_MAX) near.push_back(u);  // (one never met is never resolved)
            }
        });
        std::sort(near.begin(), near.end());
        near.erase(std::unique(near.begin(), near.end()), near.end());
    };
    // (in parallel: a list's order does not matter, it only marks variables to score again)
    std::vector<uint32_t> wstart(nv + 1, 0), wlist;
    std::vector<std::vector<uint32_t>> nears(threads());
    const size_t blocks = (nv + VARS_PER_TASK - 1) / VARS_PER_TASK;
    parallel_for(blocks, [&](size_t b, unsigned w) {
        for (uint32_t d = (uint32_t)(b * VARS_PER_TASK); d < nv && d < (b + 1) * VARS_PER_TASK; d++) {
            nearby(d, nears[w]);
            for (uint32_t u : nears[w]) std::atomic_ref<uint32_t>(wstart[u + 1]).fetch_add(1, std::memory_order_relaxed);
        }
    });
    for (uint32_t d = 0; d < nv; d++) wstart[d + 1] += wstart[d];
    wlist.resize(wstart[nv]);
    {
        std::vector<uint32_t> at(wstart.begin(), wstart.end() - 1);
        parallel_for(blocks, [&](size_t b, unsigned w) {
            for (uint32_t d = (uint32_t)(b * VARS_PER_TASK); d < nv && d < (b + 1) * VARS_PER_TASK; d++) {
                nearby(d, nears[w]);
                for (uint32_t u : nears[w]) wlist[std::atomic_ref<uint32_t>(at[u]).fetch_add(1, std::memory_order_relaxed)] = d;
            }
        });
    }
    nears = {};
    std::vector<uint8_t> dirty(nv, 1);  // to be scored in the next round
    struct Ctx {
        uint64_t before;  // up to 6 bytes before, newest lowest
        int nbefore;
        int after[AHEAD];
        int nafter;
    };
    struct Verdict {
        int bc = -1;      // the value accepted; -1: none
        std::string dbg;  // with GR_INFER_DEBUG: its line
    };
    struct Work {
        std::vector<Ctx> ctxs;
        std::vector<float> score, per;
    };
    std::vector<Work> work(threads());
    auto judge = [&](uint32_t d, int round, Work& w) -> Verdict {
        Verdict verdict;
        const uint32_t v = vt.vars[d].var;
        // the distinct contexts this byte appears in: a line copied 40 times is one piece of evidence
        std::vector<Ctx>& ctxs = w.ctxs;
        ctxs.clear();
        vt.each_place(d, [&](uint64_t, const Occ& oc) {
            Ctx c{0, 0, {}, 0};
            for (int q = 1; q <= Model::ORDERS; q++) {
                int b = val(oc.cell[q - 1], v);
                if (b < 0) break;
                c.before |= (uint64_t)b << (8 * (q - 1));
                c.nbefore = q;
            }
            for (int q = 1; q <= AHEAD; q++) {
                int b = val(oc.cell[Occ::BEFORE + q - 1], v);
                if (b < 0) break;
                c.after[q - 1] = b;
                c.nafter = q;
            }
            // Only contexts with known bytes on both sides count: a byte known only from what
            // precedes it is guessed as the commonest continuation, and measured, that misleads
            if (c.nbefore < 2 || c.nafter < 2) return;
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
        });
        if (ctxs.empty()) return verdict;
        // Every byte is a candidate: measured, keeping only the bytes seen after the byte before
        // let a value never seen there lose without a contest, and the wrong one win. Many weak
        // contexts that agree are not strong evidence, since their errors are alike (hex digits
        // in a fingerprint, say): at least one context must favour the value on its own by SOLO,
        // and none may favour another by AGAINST.
        // per context, each candidate's score; the total is their sum
        std::vector<float>& score = w.score;
        std::vector<float>& per = w.per;
        score.assign(256, 0.0f);
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
        if (!(favoured && !against && best - second >= MARGIN)) return verdict;
        verdict.bc = bc;
        if (dbg) {
            char b[96];
            std::string& line = verdict.dbg;
            snprintf(b, sizeof b, "%d %u %d %.2f %.2f %d %zu |", round, v, bc, best, second, 256, ctxs.size());
            line += b;
            for (size_t q = 0; q < ctxs.size() && q < 3; q++) {
                const Ctx& c = ctxs[q];
                line += " [";
                for (int z = c.nbefore - 1; z >= 0; z--) {
                    snprintf(b, sizeof b, "%02x", (unsigned)((c.before >> (8 * z)) & 0xff));
                    line += b;
                }
                line += "_";
                for (int z = 0; z < c.nafter; z++) {
                    snprintf(b, sizeof b, "%02x", (unsigned)c.after[z]);
                    line += b;
                }
                line += "]";
            }
            std::vector<std::pair<float, int>> top;
            for (int x = 0; x < 256; x++) top.push_back({score[x], x});
            std::sort(top.rbegin(), top.rend());
            line += " |";
            for (size_t q = 0; q < top.size() && q < 3; q++) {
                snprintf(b, sizeof b, " %02x:%.1f", top[q].second, top[q].first);
                line += b;
            }
            line += "\n";
        }
        return verdict;
    };
    for (int round = 0; round < ROUNDS && !todo.empty(); round++) {
        std::vector<uint32_t> scored;
        for (uint32_t d : todo)
            if (dirty[d]) scored.push_back(d);
        // the value each would take (-1: none), and with GR_INFER_DEBUG why
        std::vector<int16_t> verdicts(scored.size());
        std::vector<std::string> lines(dbg ? scored.size() : 0);
        parallel_for(scored.size(), [&](size_t i, unsigned w) {
            Verdict r = judge(scored[i], round, work[w]);
            verdicts[i] = (int16_t)r.bc;
            if (dbg) lines[i] = std::move(r.dbg);
        });
        // in the variables' order, as one pass over them would accept them
        std::vector<std::pair<uint32_t, int>> accepted;
        std::vector<uint32_t> left;
        size_t si = 0;
        for (uint32_t d : todo) {
            int bc = -1;
            if (si < scored.size() && scored[si] == d) {
                bc = verdicts[si];
                if (dbg && bc >= 0) fputs(lines[si].c_str(), dbg);
                si++;
            }
            dirty[d] = 0;
            if (bc >= 0) accepted.push_back({d, bc});
            else left.push_back(d);
        }
        if (accepted.empty()) break;
        for (auto& [d, c] : accepted) {
            out.value[d] = (int16_t)c;
            for (uint32_t i = wstart[d]; i < wstart[d + 1]; i++) dirty[wlist[i]] = 1;
            uint32_t u = vt.vars[d].var;
            for (uint32_t w = u > (uint32_t)Occ::BEFORE ? u - Occ::BEFORE : 1; w <= u + Occ::AFTER; w++) {
                uint32_t e = w == u ? UINT32_MAX : vt.find(w);
                if (e != UINT32_MAX) dirty[e] = 1;
            }
        }
        todo.swap(left);
    }
    if (dbg) fclose(dbg);
    uint64_t resolved = 0;
    for (uint32_t d = 0; d < nv; d++)
        if (out.value[d] >= 0) {
            done += vt.vars[d].count;
            resolved++;
        }
    if (done)
        note = std::to_string(done) + " of " + std::to_string(unknown) +
               " bytes that copied from data lost with a damaged block were inferred (" + std::to_string(resolved) +
               " lost window bytes, from where their copies appear and the file's own statistics)";
    return out;
}
// ---------------------------------------------------------------- ASCII-mode transfers

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

// When nearly every LF has a CR before it (Unix to Windows in ASCII mode; in compressed data one
// LF in 256 does by chance), the input with one CR removed before each LF
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

// The other way, Windows to Unix, every CR LF became LF: bytes were removed and nothing marks
// where. Compressed data is close to random, so CR LF occurs about once in 65,536 positions; a
// file with none of them where chance would put several dozen went through such a transfer. Or
// with a few: CR CR LF became CR LF, about one in 256 of those removed (measured: 1 left of 422
// in Python-3.14.0.tgz, 14 of 3,457 in linux-6.6.tar.gz, where "none" missed both); a sixteenth
// of what chance would put there at most.
bool looks_cr_stripped(const uint8_t* in, uint64_t size) {
    uint64_t lf = 0, cr = 0, pairs = 0;
    for (uint64_t i = 0; i < size; i++) {
        if (in[i] == '\n') {
            lf++;
            pairs += i > 0 && in[i - 1] == '\r';
        } else if (in[i] == '\r') {
            cr++;
        }
    }
    double chance = size ? (double)lf * (double)cr / (double)size : 0;
    return chance >= 8.0 && 16.0 * (double)pairs <= chance;
}

// Putting the CRs back. Every removed CR stood right before an LF byte of the input, so the
// candidates are the LF bytes, and a stream with its own CRC says when the right ones are back:
// a ZIP entry. A gzip stream has one CRC for the whole file, and nothing finer: a wrong CR makes
// a block land on the next header with the regular symbol count almost as often as the right
// one (the Huffman codes fall back in step either way; measured, 9 of 12 such repairs were
// wrong), and the text decoded after either is equally plausible (it copies other real text).
struct CrSearch {
    gi::Decoder& dec;
    std::vector<uint16_t>& scratch;
    bool full_scratch;  // as large as a search in sequence has: a full one means go on, not start over
    const uint8_t* in;
    uint64_t size, expected_size;
    uint32_t expected_crc;
    std::vector<uint8_t> base, with, bytes;  // the input with some CRs; with one more; the output
    uint64_t spent = 0;                       // input bytes decoded so far
    bool overflowed = false;                  // a smaller scratch filled up: the search must be run again in sequence
    std::vector<uint64_t>* checks = nullptr;  // when set: `spent` at each check against the budget

    // where a decode can go on from: a symbol boundary, and the output by then
    struct Snap {
        gi::Start at;
        size_t out;
        bool first;
    };

    // in with a CR before each input position in ins (sorted), zero padded; returns its length
    uint64_t build(std::vector<uint8_t>& b, const std::vector<uint64_t>& ins) const {
        b.clear();
        uint64_t p = 0;
        for (uint64_t j : ins) {
            b.insert(b.end(), in + p, in + j);
            b.push_back('\r');
            p = j;
        }
        b.insert(b.end(), in + p, in + size);
        uint64_t len = b.size();
        b.resize(len + 64, 0);
        return len;
    }

    // Decodes buffer b from snapshot s (bytes holds the output up to it) to stop_bit, or the
    // end. False: a reference before the stream's start, or more output than the stream has.
    bool go(const std::vector<uint8_t>& b, const Snap& s, uint64_t stop_bit, gi::Result& r) {
        bytes.resize(s.out);
        gi::Start st = s.at;
        bool first = s.first;
        for (;;) {
            r = dec.run(b.data(), b.size() - 64, st, nullptr, 0, scratch.data(), (uint32_t)scratch.size(), first, stop_bit);
            if (r.status == gi::ERR_OVERFLOW && !full_scratch) {
                overflowed = true;
                return false;
            }
            first = false;
            spent += (r.end_bit > st.bit ? r.end_bit - st.bit : 0) / 8;
            size_t at = bytes.size();
            bytes.resize(at + r.out_len);
            for (uint32_t i = 0; i < r.out_len; i++) {
                uint16_t v = scratch[i];
                size_t q = at + i;
                if (v <= gi::REF) {
                    bytes[q] = (uint8_t)v;
                } else if ((size_t)(v - gi::REF) <= q) {
                    bytes[q] = bytes[q - (v - gi::REF)];
                } else {
                    return false;
                }
            }
            if (bytes.size() > expected_size) return false;
            if (r.status == gi::ERR_OVERFLOW) {
                st = r.resume;
                continue;
            }
            return true;
        }
    }

    bool right(const gi::Result& r) const {
        return r.status == gi::OK && bytes.size() == expected_size && crc32(bytes.data(), bytes.size()) == expected_crc;
    }

    // For each candidate c (sorted, each after every position in fixed): is the stream right
    // with the CRs of fixed and one before c? f(c, right, where it broke as an input byte, what
    // was decoded before c's turn). The decode with fixed alone goes on from candidate to
    // candidate, and each trial starts from where it stood just before c: only the rest of the
    // stream is decoded again.
    template <class F>
    void each(const std::vector<uint64_t>& fixed, const std::vector<uint64_t>& cands, uint64_t budget, F f) {
        build(base, fixed);
        Snap cur{{0, 0, gi::AT_BLOCK, -1}, 0, true};
        bytes.clear();
        std::vector<uint64_t> ins = fixed;
        ins.push_back(0);
        for (uint64_t c : cands) {
            if (checks) checks->push_back(spent);
            if (spent > budget) return;
            uint64_t before = spent;
            uint64_t cb = c + fixed.size();  // c in the buffer
            gi::Result r;
            if (cb * 8 > cur.at.bit + 64) {
                if (!go(base, cur, cb * 8 - 64, r)) return;  // already wrong before c: so is every later trial
                // where it stopped, unless that is past c (the stop fell in a block header) or it
                // broke first (the lost CR was in the header itself): then from the one before
                if (r.status == gi::STOPPED && r.resume.bit <= cb * 8) cur = {r.resume, bytes.size(), false};
            }
            ins.back() = c;
            build(with, ins);
            bool ok = go(with, cur, UINT64_MAX, r) && right(r);
            if (overflowed) return;
            uint64_t end = r.end_bit / 8;
            f(c, ok, r.status == gi::OK ? size : end > ins.size() ? end - ins.size() : 0, before);
            if (ok) return;
        }
    }
};

// The CRs a raw stream with a known size and CRC needs back: one before an LF, or two, the
// first before the point where the decode breaks and the second before where it breaks then.
// `count`: how many it lost, when the caller knows (0: unknown). Empty when the stream is right
// as it is, or no such repair is found within the budget (input bytes decoded; `spent` says how
// many were): two CRs in a stream that never breaks cost a decode for every pair of LFs.
std::vector<uint64_t> find_lost_crs(const std::vector<uint8_t>& in, uint64_t size, uint64_t expected_size, uint32_t expected_crc,
                                    int count, uint64_t budget, uint64_t& spent) {
    spent = 0;
    if (count > 2) return {};
    Rescuer rs(in.data(), size);
    CrSearch cs{rs.dec, rs.scratch, true, in.data(), size, expected_size, expected_crc, {}, {}, {}};
    // as it is: right already, or where it breaks
    gi::Result r0;
    cs.build(cs.base, {});
    bool decoded = cs.go(cs.base, {{0, 0, gi::AT_BLOCK, -1}, 0, true}, UINT64_MAX, r0);
    spent = cs.spent;
    if (decoded && cs.right(r0)) return {};
    uint64_t err0 = r0.status == gi::OK || !decoded ? size : std::min<uint64_t>(size, r0.end_bit / 8);
    std::vector<uint64_t> lfs;
    for (uint64_t j = 0; j < size; j++)
        if (in[j] == '\n' && j <= err0) lfs.push_back(j);
    std::vector<uint64_t> found;
    std::vector<uint64_t> broke(lfs.size(), size);
    size_t at = 0;
    cs.each({}, lfs, budget, [&](uint64_t c, bool ok, uint64_t err, uint64_t) {
        if (ok) found = {c};
        while (at < lfs.size() && lfs[at] != c) at++;
        if (at < lfs.size()) broke[at] = err;
    });
    spent = cs.spent;
    if (!found.empty() || count == 1) return found;
    // The second CR, after each first: those searches are independent of one another. They run
    // in parallel, a batch at a time, each counting what it decodes from where the batch began;
    // then they are taken in order, as running one after another would take them: the first
    // that finds the pair wins, unless what was decoded before its turn passed the budget, which
    // stops the search. One that filled a worker's smaller scratch runs again here, in turn.
    auto seconds = [&](size_t x) {
        std::vector<uint64_t> second;
        for (uint64_t j = lfs[x] + 1; j <= std::min(broke[x], size - 1); j++)
            if (in[j] == '\n') second.push_back(j);
        return second;
    };
    size_t wscratch = (size_t)std::min<uint64_t>(rs.scratch.size(), expected_size + 2 * gi::MAX_SYMBOL_OUT);
    // a worker holds the input twice and the output (and its scratch): no more of them than 256 MB holds
    uint64_t per = 2 * (size + 64) + expected_size + 2 * (uint64_t)wscratch;
    unsigned nw = (unsigned)std::max<uint64_t>(1, std::min<uint64_t>(threads(), (256ull << 20) / std::max<uint64_t>(per, 1)));
    std::vector<std::unique_ptr<Verifier>> pool(nw);
    struct Try {
        bool redo = false, ok = false;
        uint64_t c = 0, total = 0;     // the pair's second CR; what it decoded in all
        std::vector<uint64_t> checks;  // what it had decoded at each check against the budget (the last: the pair's)
    };
    for (size_t x = 0; x < lfs.size() && found.empty() && cs.spent <= budget;) {
        size_t nb = std::min<size_t>(lfs.size() - x, 4 * (size_t)nw);
        std::vector<Try> tries(nb);
        uint64_t g = cs.spent;
        parallel_for(nb, [&](size_t i, unsigned w) {
            if (!pool[w]) pool[w] = std::make_unique<Verifier>(wscratch);
            Verifier& vf = *pool[w];
            CrSearch ls{vf.dec, vf.scratch, wscratch >= rs.scratch.size(), in.data(), size, expected_size, expected_crc, {}, {}, {}};
            Try& t = tries[i];
            ls.spent = g;
            ls.checks = &t.checks;
            ls.each({lfs[x + i]}, seconds(x + i), budget, [&](uint64_t c, bool ok, uint64_t, uint64_t) {
                if (!ok) return;
                t.ok = true;
                t.c = c;
            });
            for (uint64_t& v : t.checks) v -= g;
            t.redo = ls.overflowed;
            t.total = ls.spent - g;
        }, nw);
        for (size_t i = 0; i < nb && found.empty() && cs.spent <= budget; i++, x++) {
            const Try& t = tries[i];
            if (t.redo) {
                cs.each({lfs[x]}, seconds(x), budget, [&](uint64_t c, bool ok, uint64_t, uint64_t) {
                    if (ok) found = {lfs[x], c};
                });
                continue;
            }
            // in turn, the first check that finds the budget spent ends it (the worker, which
            // counted from earlier, stopped at that one or later)
            auto cut = std::find_if(t.checks.begin(), t.checks.end(), [&](uint64_t v) { return cs.spent + v > budget; });
            if (cut != t.checks.end()) {
                cs.spent += *cut;
                break;
            }
            cs.spent += t.total;
            if (t.ok) found = {lfs[x], t.c};
        }
    }
    spent = cs.spent;
    return found;
}

// The input with a CR put back before each position in crs
void put_back_crs(std::vector<uint8_t>& in, uint64_t& size, const std::vector<uint64_t>& crs) {
    std::vector<uint8_t> out;
    out.reserve(size + crs.size() + 64);
    uint64_t p = 0;
    for (uint64_t j : crs) {
        out.insert(out.end(), in.begin() + p, in.begin() + j);
        out.push_back('\r');
        p = j;
    }
    out.insert(out.end(), in.begin() + p, in.begin() + size);
    size = out.size();
    out.resize(size + 64, 0);
    in.swap(out);
}

}  // namespace

struct gr_result {
    gr_stats stats{};
    std::vector<gr_piece> pieces;
    std::vector<std::string> why, notes;
    std::vector<const char*> note_ptrs;
};

namespace {

// Writes the output in order, a chunk at a time: every piece, a suspect one cut or padded to its
// true length when known, the bytes copied from lost windows given the values inferred; and
// counts what it writes into the result
struct Emitter {
    static constexpr size_t CHUNK = 1u << 20;
    gr_sink sink;
    void* user;
    uint8_t fill;
    gr_result* res;
    const Values& value;
    FILE* dump;  // GR_DUMP_VARS: each output byte's variable
    std::vector<uint8_t> buf, mask;
    std::vector<uint32_t> vars;
    size_t n = 0;
    uint64_t pos = 0;
    int rc = GR_OK;
    uint64_t at = 0, len = 0, known = 0;  // the piece being written: where it begins, its length out, bytes not unknown

    Emitter(gr_sink s, void* u, uint8_t f, gr_result* r, const Values& v, FILE* d)
        : sink(s), user(u), fill(f), res(r), value(v), dump(d), buf(CHUNK), mask(CHUNK) {}
    ~Emitter() {
        if (dump) fclose(dump);
    }

    void put(uint8_t x, uint8_t m, uint32_t var) {
        buf[n] = x;
        mask[n] = m;
        n++;
        pos++;
        if (dump) vars.push_back(var);
        if (n == CHUNK) flush();
    }
    void flush() {
        if (n && rc == GR_OK && sink && sink(user, buf.data(), mask.data(), n)) rc = GR_ERR_ABORTED;
        n = 0;
        if (dump && !vars.empty()) fwrite(vars.data(), 4, vars.size(), dump);
        vars.clear();
    }

    void begin(const Piece& p) {
        at = pos;
        len = p.suspect && p.true_len ? p.true_len : p.len;
        known = 0;
    }
    // bytes [from, to) of piece p: b and kn start at from; var_at(j), for each unknown byte j in
    // turn, the variable it copies
    template <class VarAt>
    void bytes(const Piece& p, const uint8_t* b, const uint8_t* kn, VarAt&& var_at, uint64_t from, uint64_t to) {
        gr_stats& s = res->stats;
        for (uint64_t i = from; i < std::min(to, len); i++) {
            size_t j = (size_t)(i - from);
            uint8_t k = kn[j], x = b[j];
            uint32_t v = k ? 0 : var_at(j);
            if (k == 0 && v) {
                int got = value.get(v);
                if (got >= 0) {
                    x = (uint8_t)got;
                    k = 2;
                }
            }
            uint32_t dv = k == 1 ? 0 : v;
            if (!k) {
                s.unknown_bytes++;
                put(fill, GR_UNKNOWN, dv);
                continue;
            }
            known++;
            if (p.suspect) {
                s.suspect_bytes++;
                put(x, GR_SUSPECT, dv);
            } else if (k == 2) {
                put(x, GR_INFERRED, dv);
            } else {
                s.exact_bytes++;
                put(x, GR_EXACT, dv);
            }
        }
    }
    void end(const Piece& p) {
        if (len > p.len) {
            res->stats.unknown_bytes += len - p.len;
            for (uint64_t i = p.len; i < len; i++) put(fill, GR_UNKNOWN, 0);
        }
        res->why.push_back(p.why);
        res->pieces.push_back(gr_piece{at, len, p.len, known, p.in_bit_start / 8, p.in_bit_end / 8, p.suspect ? 1 : 0, p.status,
                                       p.blocks, p.symbols, nullptr});
        if (p.suspect) res->stats.damaged++;
    }
};

}  // namespace

extern "C" {

GR_API uint32_t gr_abi_version(void) { return GR_ABI_VERSION; }

static int rescue_common(const uint8_t* in_, uint64_t size, bool gzip, uint64_t expected_size, bool has_crc, uint32_t expected_crc,
                         const gr_options* opt, gr_sink sink, void* user, gr_result** result) {
    if (result) *result = nullptr;
    if (!in_ && size) return GR_ERR_ARG;
    uint8_t fill = opt && opt->size >= 5 ? opt->fill : 0;
    bool infer = !(opt && opt->size >= 6 && opt->no_infer);
    uint64_t max_output = opt && opt->size >= 16 ? opt->max_output : 0;
    try {
        std::vector<uint8_t> in(size + 64, 0);  // zero padding: the readers look ahead
        if (size) memcpy(in.data(), in_, size);
        bool ascii_undone = undo_lf_to_crlf(in, size);
        uint64_t zero_tail = cut_zero_tail(size, in);
        // a Windows-to-Unix ASCII transfer: CRs put back where they make damaged blocks whole
        uint8_t cr_hint = opt && opt->size >= 7 ? opt->cr_lost : 0;
        uint8_t crs_expected = opt && opt->size >= 8 ? opt->crs_expected : 0;
        bool cr_stripped = !ascii_undone && (cr_hint == 1 || (cr_hint == 0 && looks_cr_stripped(in.data(), size)));
        uint64_t crs_restored = 0, cr_decoded = 0;
        if (cr_stripped && !gzip && has_crc && expected_size != UINT64_MAX && crs_expected != 255) {
            uint64_t budget = opt && opt->size >= 24 && opt->cr_budget ? opt->cr_budget : CR_BUDGET;
            std::vector<uint64_t> crs = find_lost_crs(in, size, expected_size, expected_crc, crs_expected, budget, cr_decoded);
            crs_restored = crs.size();
            if (!crs.empty()) put_back_crs(in, size, crs);
        }
        Rescuer rs(in.data(), size);
        // A stream that lost its CRs has damage in two blocks of five, and as many lost windows,
        // whose bytes are copied everywhere: of their places only those that tell scoring something
        // are kept (measured: linux-6.6.tar.gz, 1,062 damaged blocks, 45.9 million places, of them
        // 19.5 million kept). Inference comes out the same; it could differ only where a damaged
        // block is split later at a run of zeros, which blanks the cells next to the split.
        rs.drop_alike = cr_stripped && !crs_restored && gzip;
        uint64_t hlen = gzip ? gzip_header(in.data(), size, 0) : 0;
        std::vector<gi::Start> headers = rs.find_headers(gzip ? (hlen ? hlen * 8 : 80) : 1);
        if (gzip && headers.empty() && !hlen) return GR_ERR_NO_DEFLATE;
        if (!gzip && size == 0) return GR_ERR_NO_DEFLATE;
        // first pass; a second one when blocks that stayed in step have the wrong symbol count, and
        // another when a member's CRC fails with every block in step and the damage can be placed
        rs.max_output = max_output;
        Outcome o = rs.rescue(headers, gzip, hlen, {}, expected_size, has_crc, expected_crc);
        std::string anomaly_note;
        Habit habit;
        std::map<uint64_t, std::string> forced;
        bool crs_lost = cr_stripped && !crs_restored;
        std::vector<char> whole(o.pieces.size(), 0);
        std::set<uint64_t> odd = symbol_count_anomalies(o, whole, crs_lost, anomaly_note, habit);
        // A member whose CRC checks over this pass is whole: counts that stand out in it are its
        // encoder's. One of several pieces whose CRC fails, every block in step, holds damage the
        // blocks flagged for their count may not be all of (measured: linux-6.6.tar.gz with three
        // zeroed sectors, one flagged so, the others in step with a count that fits; 43 wrong
        // files would have been called intact); a member of one piece is placed by crc_misses.
        bool crc_failed = false;
        if (!odd.empty()) {
            bool any = false;
            for (const Member& m : o.members) {
                int c = member_crc(o, m);
                crc_failed |= c == 0 && m.last_piece > m.first_piece;
                if (c == 1) {
                    for (size_t k = m.first_piece; k <= m.last_piece && k < whole.size(); k++) whole[k] = 1;
                    any = true;
                }
            }
            if (any) {
                anomaly_note.clear();
                odd = symbol_count_anomalies(o, whole, crs_lost, anomaly_note, habit);
            }
        }
        // Damage that keeps the decoder in step shows, when at all, in a block's count: a flipped
        // bit, a lost byte. Where it showed with no run of zeros or 0xFF bytes in the block to
        // account for it (bit rot, not a zeroed sector), damage of the same kind may lie in blocks
        // whose count it left as it was (measured: 5 of 8 flipped bits in Python-3.14.0.tgz); a
        // transfer that removed CRs leaves blocks like that too (40 of the 280 that lost one).
        // Other data written over a cluster can decode in step too, and is taken the same way.
        std::string hidden;  // what showed such damage
        if (crs_lost && gzip) hidden = "CRs removed by an ASCII-mode transfer";
        else if (crc_failed) hidden = "the CRC fails on the stream as decoded, every block in step";
        auto no_run = [&](const Piece& p) {
            uint64_t n = 0;
            uint8_t v = 0;
            return rs.first_run(p.in_bit_start / 8, (p.in_bit_end + 7) / 8 + 8, n, v) == UINT64_MAX;
        };
        for (const Piece& p : o.pieces)
            if (hidden.empty() && odd.count(p.in_bit_start) && no_run(p))
                hidden = "a block decoded in step with a symbol count its encoder never writes, and no run of zeros in it";
        for (uint64_t b : odd) forced[b] = "its symbol count differs from the stream's regular one";
        if (!forced.empty() && !o.too_big) {
            o = {};
            o = rs.rescue(headers, gzip, hlen, forced, expected_size, has_crc, expected_crc);
        }
        std::vector<std::string> crc_notes;
        std::map<uint64_t, std::string> placed = rs.crc_misses(o, crc_notes);
        if (!placed.empty() && !o.too_big) {
            forced.insert(placed.begin(), placed.end());
            o = {};
            o = rs.rescue(headers, gzip, hlen, forced, expected_size, has_crc, expected_crc);
            for (Piece& p : o.pieces) p.by_crc = placed.count(p.in_bit_start) > 0;
        }
        if (o.too_big) return GR_ERR_TOO_BIG;
        if (!anomaly_note.empty()) o.notes.push_back(anomaly_note);
        o.notes.insert(o.notes.end(), crc_notes.begin(), crc_notes.end());
        // bit rot that broke a decode: one flipped bit mends the block
        int looked = 0;
        for (const Piece& p : o.pieces) {
            if (!hidden.empty() || looked >= FLIP_PIECES) break;
            if (!p.suspect || p.by_crc || p.status != gi::ERR_DATA || !no_run(p)) continue;
            looked++;
            if (rs.one_flip_mends(p, headers, in.data(), habit)) hidden = "a damaged block that one flipped bit mends";
        }
        rs.suspect_overfull_tail(o, habit.regular);
        rs.localize(o, headers);
        if (ascii_undone) o.notes.push_back("the file had gone through an ASCII-mode transfer (every LF became CR LF): undone");
        if (crs_restored)
            o.notes.push_back(std::to_string(crs_restored) + " CR" + (crs_restored == 1 ? "" : "s") +
                              " put back that an ASCII-mode transfer had removed; the CRC confirms it");
        else if (cr_stripped && gzip)
            o.notes.push_back("the file went through an ASCII-mode transfer that removed the CR of every CR LF (it has almost no CR LF "
                              "pair where chance would put dozens): the blocks it broke are flagged; the CRs cannot be put back "
                              "here, with one CRC for the whole file to judge them. A copy transferred in binary mode is whole");
        if (zero_tail)
            o.notes.push_back("the file ends in " + std::to_string(zero_tail) +
                              " zero bytes (written by a crash or an unfinished download, not by gzip): the data ends where they begin");
        std::vector<Piece>& pieces = o.pieces;
        uint64_t inferred = 0;
        Values value;  // the lost window bytes inferred
        if (infer) {
            std::string note;
            value = infer_window(o, rs, headers, inferred, note);
            if (!note.empty()) o.notes.push_back(note);
        }
        gr_result* res = new gr_result;
        std::unique_ptr<gr_result> owned(res);
        res->stats.input_size = size;
        res->stats.headers = (uint32_t)headers.size();
        res->stats.members = (uint32_t)o.members.size();
        res->stats.truncated = o.truncated;
        res->stats.header_damaged = gzip && hlen == 0;
        res->stats.crc_check = -1;
        res->stats.input_end = o.input_end;
        res->stats.inferred_bytes = inferred;
        res->stats.ascii_undone = ascii_undone;
        res->stats.cr_restored = (uint32_t)crs_restored;
        res->stats.cr_decoded = cr_decoded;

        // The true length of each suspect piece: the trailer's size minus what is certain, spread
        // over the suspect pieces by their compressed size when there are several. ISIZE counts
        // modulo 4 GiB; the difference is believed when the damaged blocks could plausibly hold it.
        for (Member& m : o.members) {
            uint64_t good = 0, suspect_bits = 0, sus_decoded = 0;
            uint64_t sized = 0;  // bytes of suspect pieces whose length is right
            int nsus = 0, nspread = 0;
            for (size_t k = m.first_piece; k <= m.last_piece && k < pieces.size(); k++) {
                const Piece& p = pieces[k];
                if (p.suspect) nsus++;
                if (p.suspect && !p.length_exact) {
                    nspread++;
                    sus_decoded += p.len;
                    suspect_bits += p.in_bit_end > p.in_bit_start ? p.in_bit_end - p.in_bit_start : 0;
                } else if (p.suspect) {
                    sized += p.len;
                } else {
                    good += p.len;
                }
            }
            uint32_t missing = m.isize - (uint32_t)(good + sized);
            if (m.trailer && nspread > 0 && missing <= 4 * sus_decoded + (64u << 20)) {
                uint64_t given = 0;
                int seen = 0;
                for (size_t k = m.first_piece; k <= m.last_piece && k < pieces.size(); k++) {
                    if (!pieces[k].suspect || pieces[k].length_exact) continue;
                    seen++;
                    uint64_t bits = pieces[k].in_bit_end > pieces[k].in_bit_start ? pieces[k].in_bit_end - pieces[k].in_bit_start : 0;
                    pieces[k].true_len = seen == nspread ? missing - given : (suspect_bits ? (uint64_t)missing * bits / suspect_bits : 0);
                    given += pieces[k].true_len;
                }
            }
            if (m.trailer && nsus == 0 && !pieces.empty() && pieces[m.last_piece].status == gi::OK) {
                // every piece in step, none split: the member's CRC is that of what was decoded
                bool all_known = true;
                for (size_t k = m.first_piece; k <= m.last_piece; k++) all_known &= pieces[k].all_exact;
                if (all_known) {
                    uint32_t c = m.out_crc;
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
        // (A block flagged because the CRC failed has its own note.)
        if (res->stats.crc_check == -1) {
            bool damaged = false;
            for (const Piece& p : pieces) damaged |= p.suspect && !p.by_crc;
            if (damaged && !hidden.empty())
                o.notes.push_back("with damaged blocks in it, the stream's CRC cannot be checked, and its damage is of a kind that can "
                                  "keep the decoder in step (" + hidden + "): damage like that elsewhere would not show, so the bytes "
                                  "marked exact outside the flagged blocks are not vouched for");
            else if (damaged)
                o.notes.push_back("with damaged blocks in it, the stream's CRC cannot be checked: bytes marked exact are exact as far as "
                                  "decoding can tell, and damage that kept the decoder in step (a flipped bit that changed a literal "
                                  "or a match length) would not show");
        }
        res->stats.hidden_damage = !hidden.empty() && res->stats.crc_check != 1;
        uint64_t out_size = 0;
        for (const Piece& p : pieces) out_size += p.suspect && p.true_len ? p.true_len : p.len;
        if (max_output && out_size > max_output) return GR_ERR_TOO_BIG;

        // the output, in order: the pieces held from memory, then the rest decoded again
        // GR_DUMP_VARS=<file>: the variable of every output byte (uint32, 0: none), for checking
        const char* dv = getenv("GR_DUMP_VARS");
        Emitter em(sink, user, fill, res, value, dv ? fopen(dv, "wb") : nullptr);
        size_t cur = 0;
        for (; cur < pieces.size() && pieces[cur].src < o.unheld; cur++) {
            const Piece& p = pieces[cur];
            em.begin(p);
            size_t u = 0;
            em.bytes(p, p.bytes.data(), p.known.data(), [&](size_t) { return p.uvar[u++]; }, 0, p.len);
            em.end(p);
        }
        if (o.unheld != SIZE_MAX) {
            bool open = false;  // pieces[cur] has begun
            bool same = rs.replay(o, headers, o.unheld, o.sources.size(), [&](size_t k, uint64_t a, uint64_t b, bool) {
                const Rescuer::Tail& t = rs.tail;
                while (a < b) {
                    // the pieces before this one, or of it but ending by a, are done (empty ones too)
                    while (cur < pieces.size() && (pieces[cur].src < k || pieces[cur].src_off + pieces[cur].len <= a)) {
                        if (!open) em.begin(pieces[cur]);
                        em.end(pieces[cur]);
                        open = false;
                        cur++;
                    }
                    const Piece& p = pieces[cur];
                    if (!open) {
                        em.begin(p);
                        open = true;
                    }
                    uint64_t e = std::min(b, p.src_off + p.len);
                    size_t q = (size_t)(a - t.base);
                    const uint32_t* tv = t.var.data() + q;
                    em.bytes(p, t.bytes.data() + q, t.known.data() + q, [&](size_t j) { return tv[j]; }, a - p.src_off, e - p.src_off);
                    a = e;
                    if (e == p.src_off + p.len) {
                        em.end(p);
                        open = false;
                        cur++;
                    }
                }
            });
            if (!same) throw std::runtime_error("a decode came out differently the second time");
            for (; cur < pieces.size(); cur++, open = false) {
                if (!open) em.begin(pieces[cur]);
                em.end(pieces[cur]);
            }
        }
        em.flush();
        int rc = em.rc;
        uint64_t pos = em.pos;
        for (size_t k = 0; k < res->pieces.size(); k++) res->pieces[k].why = res->why[k].c_str();
        res->stats.output_size = pos;
        res->stats.pieces = (uint32_t)res->pieces.size();
        res->notes = o.notes;
        for (const std::string& n : res->notes) res->note_ptrs.push_back(n.c_str());
        owned.release();
        if (result) *result = res;
        else gr_free(res);
        return rc;
    } catch (const std::bad_alloc&) {
        return GR_ERR_MEMORY;
    } catch (const std::runtime_error&) {
        return GR_ERR_INTERNAL;
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
