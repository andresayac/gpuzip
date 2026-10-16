// gpuinflate - prototype: decompress ONE raw Deflate stream in parallel by speculative
// decoding, with CPU threads and on the GPU, and check both against the expected bytes.
//
//   gpuinflate <raw-deflate-file> <expected-output-file> [chunk_kb=4]
//
// (1) find a plausible dynamic-block start in every chunk of the compressed stream (a cheap
// test on every bit position, then zlib's full header checks on the survivors); (2) inside
// long stretches without one (fixed-Huffman blocks have no header to find), probe: decode
// from a few KB before a point and take the first symbol boundary after it, relying on
// Huffman codes resynchronizing; (3) decode from every start at once until landing on a later
// start, writing bytes and back-references instead of copying; a decoder whose buffer fills
// stops cleanly and a new one continues from there; (4) stitch the pieces that connect,
// starting from bit 0; (5) turn every back-reference into an absolute position and resolve
// them all by pointer jumping.
#include "inflate.cuh"

#include <cuda_runtime.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <numeric>
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

using clk = std::chrono::steady_clock;
static double since(clk::time_point t) { return std::chrono::duration<double>(clk::now() - t).count(); }

constexpr uint64_t SLACK = 65536;          // output symbols every job gets beyond its share
constexpr uint32_t LIT = 0x80000000u;      // resolved entry: a byte (low 8 bits); else the position it copies
constexpr int HOPS = 8;                    // pointer-jumping steps per entry per round
constexpr uint64_t PROBE_LEAD = 4096 * 8;  // a probe starts decoding this many bits before its point
constexpr int MAX_ROUNDS = 64;             // rounds of continuations before giving up

// the stream, zero-padded as the decoder expects
static std::vector<uint8_t> read_file(const char* path, size_t& size) {
    FILE* f = fopen(path, "rb");
    if (!f) {
        fprintf(stderr, "cannot open %s\n", path);
        exit(1);
    }
    fseek(f, 0, SEEK_END);
    size = (size_t)_ftelli64(f);
    fseek(f, 0, SEEK_SET);
    std::vector<uint8_t> v((size + 3) / 4 * 4 + 32, 0);
    if (size && fread(v.data(), 1, size, f) != size) exit(1);
    fclose(f);
    return v;
}

// ---------------------------------------------------------------- planning (shared by CPU and GPU)

// the block starts found, one per chunk at most, with the stream start first
static std::vector<gi::Start> block_starts(const uint64_t* first, uint64_t nchunks) {
    std::vector<gi::Start> v{{0, 0, gi::AT_BLOCK, -1}};
    for (uint64_t c = 1; c < nchunks; c++)
        if (first[c] != UINT64_MAX && first[c] > v.back().bit) v.push_back({first[c], first[c], gi::AT_BLOCK, -1});
    return v;
}

// points to probe inside stretches longer than 2 * spacing without a block start, each with
// the header the stretch begins with (whose tables the probe decodes with)
struct Probe {
    uint64_t header, point;
};

static std::vector<Probe> probe_points(const std::vector<gi::Start>& s, uint64_t nbits, uint64_t spacing) {
    std::vector<Probe> pts;
    for (size_t k = 0; k < s.size(); k++) {
        uint64_t end = k + 1 < s.size() ? s[k + 1].bit : nbits;
        if (end - s[k].bit < 2 * spacing) continue;
        for (uint64_t b = s[k].bit + spacing; b + spacing / 2 < end; b += spacing) pts.push_back({s[k].bit, b});
    }
    return pts;
}

static std::vector<gi::Start> merge_starts(std::vector<gi::Start> s, const std::vector<gi::Start>& probed,
                                           uint64_t nbits) {
    for (const gi::Start& e : probed)
        if (e.bit < nbits) s.push_back(e);
    std::stable_sort(s.begin(), s.end(), [](const gi::Start& a, const gi::Start& b) { return a.bit < b.bit; });
    std::vector<gi::Start> out;
    for (auto& x : s)
        if (out.empty() || x.bit != out.back().bit) out.push_back(x);
    return out;
}

// Decoding jobs: first one per target (start), then continuations of jobs whose buffer filled
struct Jobs {
    std::vector<gi::Start> targets;  // every start found, by bit; job k < targets.size() starts at targets[k]
    std::vector<gi::Start> start;
    std::vector<int> tfirst;         // a job may land on targets[tfirst..]
    std::vector<uint64_t> off;       // its output region in the 16-bit buffer
    std::vector<uint32_t> cap;
    std::vector<gi::Result> res;
    std::vector<int> cont;           // the job that continues it after an overflow, or -1
    uint64_t used = 0;               // 16-bit buffer in use
    uint64_t pool = 0;               // 16-bit buffer size

    void add(const gi::Start& s, int tf, uint64_t c) {
        start.push_back(s);
        tfirst.push_back(tf);
        off.push_back(used);
        cap.push_back((uint32_t)std::min<uint64_t>(c, 0xfffff000ull));
        used += cap.back();
        res.push_back({});
        cont.push_back(-1);
    }
};

// one job per start; buffers sized from the stream's overall ratio, twice over
static Jobs make_jobs(std::vector<gi::Start> targets, uint64_t nbits, double ratio, uint64_t pool) {
    Jobs j;
    j.pool = pool;
    j.targets = std::move(targets);
    size_t n = j.targets.size();
    for (size_t k = 0; k < n; k++) {
        uint64_t span = (k + 1 < n ? j.targets[k + 1].bit : nbits) - j.targets[k].bit;
        j.add(j.targets[k], (int)k + 1, (uint64_t)(2 * ratio * (double)(span / 8)) + SLACK);
    }
    return j;
}

// a continuation for every job that overflowed without one; returns the first new job
static size_t add_continuations(Jobs& j, size_t from) {
    size_t first = j.start.size();
    for (size_t k = from; k < first; k++) {
        if (j.res[k].status != gi::ERR_OVERFLOW || j.cont[k] >= 0) continue;
        const gi::Start& r = j.res[k].resume;
        int tf = (int)(std::lower_bound(j.targets.begin(), j.targets.end(), r.bit,
                                        [](const gi::Start& a, uint64_t b) { return a.bit < b; }) -
                       j.targets.begin());
        uint64_t c = std::max<uint64_t>(4ull * j.cap[k], 1u << 20);
        c = std::min(c, j.pool - j.used);
        if (c < 2 * gi::MAX_SYMBOL_OUT) break;  // out of buffer: the stitch will fail
        j.cont[k] = (int)j.start.size();
        j.add(r, tf, c);
    }
    return first;
}

struct Pieces {
    std::vector<uint64_t> off;  // outputs of the pieces that chain from bit 0 to the end
    std::vector<uint32_t> len;
    std::vector<uint64_t> dst;  // where each piece goes in the final output
    uint64_t total = 0;
};

enum Walk { DONE, NEED, FAIL };

// Follow the pieces from bit 0: each landing names the job that goes on from there, each
// overflow its continuation. NEED: a continuation is not decoded yet.
static Walk stitch(const Jobs& j, Pieces& s) {
    s = Pieces{};
    bool debug = getenv("GI_DEBUG") != nullptr;
    int k = 0;
    int final = 0;  // BFINAL of the block the next piece starts in
    for (size_t steps = 0; steps <= j.start.size(); steps++) {
        const gi::Result& r = j.res[k];
        const gi::Start& st = j.start[k];
        // a piece that starts inside the last block, unaware of it, ends at its end-of-block code
        bool last = st.mode != gi::AT_BLOCK && st.final < 0 && final == 1 && r.eob_bit != UINT64_MAX;
        if (debug && (r.status != gi::OK || r.landed < 0 || last))
            fprintf(stderr, "  job #%d at bit %llu mode %d: status %d, %u symbols, landed %d, end_final %d\n", k,
                    (unsigned long long)st.bit, st.mode, r.status, r.out_len, r.landed, r.end_final);
        uint32_t len = last ? r.eob_out : r.out_len;
        if (!last && r.status != gi::OK && r.status != gi::ERR_OVERFLOW) {
            fprintf(stderr, "  job #%d at bit %llu failed: status %d after %u symbols\n", k, (unsigned long long)st.bit,
                    r.status, r.out_len);
            return FAIL;
        }
        s.off.push_back(j.off[k]);
        s.len.push_back(len);
        s.dst.push_back(s.total);
        s.total += len;
        if (r.end_final >= 0) final = r.end_final;
        if (last) return DONE;
        if (r.status == gi::ERR_OVERFLOW) {
            if (j.cont[k] < 0 || j.res[j.cont[k]].status < 0) return NEED;
            k = j.cont[k];
            continue;
        }
        if (r.landed < 0) return DONE;
        k = j.tfirst[k] + r.landed;
    }
    return FAIL;
}

// entry for output position pos, from the piece's 16-bit symbol v
GI_INL uint32_t absolute(uint16_t v, uint64_t pos) {
    if (v <= gi::REF) return LIT | v;
    uint64_t d = v - gi::REF;
    return d <= pos ? (uint32_t)(pos - d) : LIT;  // before the stream: never in valid data
}

// follow e's chain up to HOPS steps in the entries array
GI_INL uint32_t jump(const uint32_t* e32, uint32_t e) {
    for (int h = 0; h < HOPS && !(e & LIT); h++) e = e32[e];
    return e;
}

// Decode every job, adding continuations until the pieces chain to the end. decode(a, b)
// decodes jobs a..b-1 in parallel. Returns the rounds used, or -1.
static int decode_all(Jobs& j, Pieces& s, const std::function<void(size_t, size_t)>& decode) {
    size_t a = 0;
    for (int round = 1; round <= MAX_ROUNDS; round++) {
        for (size_t k = a; k < j.start.size(); k++) j.res[k].status = -1;  // not decoded yet
        decode(a, j.start.size());
        Walk w = stitch(j, s);
        if (w == DONE) return round;
        if (w == FAIL) return -1;
        a = add_continuations(j, a == 0 ? 0 : a);
        if (a == j.start.size()) return -1;
    }
    return -1;
}

// ---------------------------------------------------------------- CPU (threads)

template <class F>
static void cpu_parallel(uint64_t n, int threads, F&& f) {
    std::vector<std::thread> pool;
    std::atomic<uint64_t> next{0};
    for (int t = 0; t < threads; t++)
        pool.emplace_back([&] {
            for (uint64_t i; (i = next.fetch_add(1)) < n;) f(i);
        });
    for (auto& th : pool) th.join();
}

static uint64_t bits64(const uint8_t* in, uint64_t bit) {
    uint64_t lo, hi;
    memcpy(&lo, in + (bit >> 3), 8);
    memcpy(&hi, in + (bit >> 3) + 8, 8);
    int off = (int)(bit & 7);
    return off ? (lo >> off) | (hi << (64 - off)) : lo;
}

struct CpuDecoder {
    gi::Decoder dec;
    uint16_t luts[gi::LUT_U16];
    CpuDecoder() { dec.attach(luts); }
};

static bool cpu_speculative(const std::vector<uint8_t>& in, size_t size, const std::vector<uint8_t>& expect,
                            size_t expect_size, uint64_t chunk_bits, int threads) {
    uint64_t nbits = (uint64_t)size * 8;
    uint64_t nchunks = (nbits + chunk_bits - 1) / chunk_bits;
    double ratio = (double)expect_size / (double)std::max<size_t>(size, 1);
    std::vector<uint64_t> first(nchunks, UINT64_MAX);
    std::vector<uint32_t> e32(expect_size);
    std::vector<uint8_t> out(expect_size);
    auto t0 = clk::now();
    first[0] = 0;
    cpu_parallel(nchunks - 1, threads, [&](uint64_t i) {
        uint64_t c = i + 1;
        for (uint64_t b = c * chunk_bits; b < std::min(nbits, (c + 1) * chunk_bits); b++) {
            uint64_t v1 = bits64(in.data(), b);
            if (gi::header_prefilter(v1) && gi::header_prefilter128(v1, bits64(in.data(), b + 64)) &&
                gi::plausible_dynamic_header(in.data(), size, b)) {
                first[c] = b;
                break;
            }
        }
    });
    std::vector<gi::Start> starts = block_starts(first.data(), nchunks);
    std::vector<Probe> pts = probe_points(starts, nbits, 4 * chunk_bits);
    std::vector<gi::Start> probed(pts.size());
    cpu_parallel(pts.size(), threads, [&](uint64_t i) {
        thread_local CpuDecoder d;
        probed[i] = d.dec.probe(in.data(), size, pts[i].header, pts[i].point - PROBE_LEAD, pts[i].point);
    });
    starts = merge_starts(std::move(starts), probed, nbits);
    double t_find = since(t0);
    auto t1 = clk::now();
    Jobs j = make_jobs(starts, nbits, ratio, 0);
    j.pool = j.used + 2 * expect_size + starts.size() * SLACK;
    std::vector<uint16_t> out16(j.pool);
    size_t n0 = j.start.size();
    std::vector<int> order(n0);
    std::iota(order.begin(), order.end(), 0);
    std::sort(order.begin(), order.end(), [&](int a, int b) { return j.cap[a] > j.cap[b]; });
    Pieces s;
    int rounds = decode_all(j, s, [&](size_t a, size_t b) {
        cpu_parallel(b - a, threads, [&](uint64_t i) {
            size_t k = a == 0 ? order[i] : a + i;
            thread_local CpuDecoder d;
            j.res[k] = d.dec.run(in.data(), size, j.start[k], j.targets.data() + j.tfirst[k],
                                 (int)j.targets.size() - j.tfirst[k], out16.data() + j.off[k], j.cap[k], k == 0);
        });
    });
    double t_decode = since(t1);
    if (rounds < 0 || s.total != expect_size) {
        fprintf(stderr, "  CPU output is %llu bytes, expected %zu\n", (unsigned long long)s.total, expect_size);
        return false;
    }
    auto t2 = clk::now();
    uint64_t np = s.off.size();
    cpu_parallel(np, threads, [&](uint64_t q) {
        for (uint32_t i = 0; i < s.len[q]; i++) e32[s.dst[q] + i] = absolute(out16[s.off[q] + i], s.dst[q] + i);
    });
    constexpr uint64_t BLK = 1 << 16;
    int jumps = 0;
    for (std::atomic<bool> again{true}; again; jumps++) {
        again = false;
        cpu_parallel((expect_size + BLK - 1) / BLK, threads, [&](uint64_t b) {
            bool left = false;
            for (uint64_t i = b * BLK; i < std::min<uint64_t>(expect_size, (b + 1) * BLK); i++) {
                uint32_t e = e32[i];
                if (e & LIT) continue;
                e = jump(e32.data(), e);
                e32[i] = e;
                left |= !(e & LIT);
            }
            if (left) again = true;
        });
    }
    cpu_parallel((expect_size + BLK - 1) / BLK, threads, [&](uint64_t b) {
        for (uint64_t i = b * BLK; i < std::min<uint64_t>(expect_size, (b + 1) * BLK); i++) out[i] = (uint8_t)e32[i];
    });
    double t_resolve = since(t2);
    double total = since(t0);
    bool good = memcmp(out.data(), expect.data(), expect_size) == 0;
    printf("CPU speculative, %2d threads: %6.3f s = %5.2f GB/s  [find+probe %.1f · decode %.1f (%d rounds) · resolve %.1f ms]"
           "  starts %zu (%zu probes), jobs %zu, pieces %llu  %s\n",
           threads, total, expect_size / total / 1e9, t_find * 1e3, t_decode * 1e3, rounds, t_resolve * 1e3, n0,
           pts.size(), j.start.size(), (unsigned long long)np, good ? "OK" : "WRONG OUTPUT");
    return good;
}

// ---------------------------------------------------------------- GPU

// Finder, pass 1: every bit position of the stream gets the same cheap test (one thread per
// 32-bit word, 32 offsets each); survivors are appended to a list
__global__ void k_prefilter(const uint32_t* w, uint64_t nwords, uint64_t nbits, uint64_t* cand, unsigned* ncand,
                            unsigned maxcand) {
    uint64_t word = blockIdx.x * (uint64_t)blockDim.x + threadIdx.x;
    if (word >= nwords) return;
    uint64_t a = __ldg(w + word), b = __ldg(w + word + 1), c = __ldg(w + word + 2), d = __ldg(w + word + 3);
    uint64_t lo = a | b << 32, hi = c | d << 32;
    for (int o = 0; o < 32; o++) {
        uint64_t bit = word * 32 + o;
        if (bit == 0 || bit >= nbits) continue;
        uint64_t v1 = o ? (lo >> o) | (hi << (64 - o)) : lo;
        uint64_t v2 = hi >> o;
        if (gi::header_prefilter128(v1, v2)) {
            unsigned i = atomicAdd(ncand, 1u);
            if (i < maxcand) cand[i] = bit;
        }
    }
}

// Finder, pass 2: the full header check for the survivors; each chunk keeps its first hit
__global__ void k_verify(const uint8_t* in, uint64_t size, const uint64_t* cand, unsigned ncand, uint64_t chunk_bits,
                         unsigned long long* first) {
    unsigned i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= ncand) return;
    uint64_t bit = cand[i];
    uint64_t c = bit / chunk_bits;
    if (bit >= first[c]) return;
    if (gi::plausible_dynamic_header(in, size, bit)) atomicMin(&first[c], (unsigned long long)bit);
}

constexpr int LUT_STRIDE = gi::LUT_U16 + 2;  // odd word count per decoder: fewer shared-memory bank conflicts
constexpr int DECODE_BLOCK = 256;            // threads per block: 8 warps, 8 decoders
// Registers are allocated for all 32 lanes of a decoder's warp, so they decide how many
// decoders fit on an SM at once; this caps them (65536 per SM / (blocks * 256 threads)).
#ifndef GI_DECODE_BLOCKS_PER_SM
#define GI_DECODE_BLOCKS_PER_SM 4
#endif

// One decoder per warp, on lane 0: lanes of one warp never decode different streams, which
// would make them wait for each other on every branch (5x slower, measured).
__global__ void __launch_bounds__(DECODE_BLOCK, GI_DECODE_BLOCKS_PER_SM)
k_probe(const uint8_t* in, uint64_t size, const Probe* pts, int n, gi::Start* probed) {
    extern __shared__ uint16_t luts[];
    int w = (blockIdx.x * blockDim.x + threadIdx.x) >> 5;
    if (w >= n || (threadIdx.x & 31)) return;
    gi::Decoder dec;
    dec.attach(luts + (threadIdx.x >> 5) * LUT_STRIDE);
    probed[w] = dec.probe(in, size, pts[w].header, pts[w].point - PROBE_LEAD, pts[w].point);
}

// the stored blocks the decoders skipped, one block of threads per stored block
__global__ void k_stored(const gi::StoredCopy* list, unsigned n) {
    if (blockIdx.x >= n) return;
    gi::StoredCopy c = list[blockIdx.x];
    for (uint32_t i = threadIdx.x; i < c.len; i += blockDim.x) c.dst[i] = c.src[i];
}

// Warps take jobs a.. (through `order` for the first round: longest first)
__global__ void __launch_bounds__(DECODE_BLOCK, GI_DECODE_BLOCKS_PER_SM)
k_decode(const uint8_t* in, uint64_t size, const gi::Start* jobs, const int* tfirst,
                         const uint64_t* off, const uint32_t* cap, const gi::Start* targets, int ntargets,
                         const int* order, int a, int n, uint16_t* out, gi::Result* res, long long* cycles,
                         gi::StoredCopies stored) {
    extern __shared__ uint16_t luts[];
    int w = (blockIdx.x * blockDim.x + threadIdx.x) >> 5;
    if (w >= n || (threadIdx.x & 31)) return;
    int k = order ? order[w] : a + w;
    long long t = clock64();
    gi::Decoder dec;
    dec.attach(luts + (threadIdx.x >> 5) * LUT_STRIDE);
    res[k] = dec.run(in, size, jobs[k], targets + tfirst[k], ntargets - tfirst[k], out + off[k], cap[k], k == 0,
                     &stored);
    cycles[k] = clock64() - t;
}

// every output position as a byte or the absolute position it copies (one block per piece)
__global__ void k_absolute(const uint16_t* out16, const uint64_t* poff, const uint32_t* plen, const uint64_t* pdst,
                           uint32_t* e32) {
    int q = blockIdx.x;
    const uint16_t* src = out16 + poff[q];
    uint64_t dst = pdst[q];
    for (uint32_t i = threadIdx.x; i < plen[q]; i += blockDim.x) e32[dst + i] = absolute(src[i], dst + i);
}

// One round of pointer jumping over the entries in `list` (all of them when list is null):
// each follows its chain up to HOPS steps, in place; the ones still unresolved go to `next`.
__global__ void k_jump(uint32_t* e32, uint64_t n, const uint32_t* list, uint32_t* next, unsigned* nnext) {
    uint64_t t = blockIdx.x * (uint64_t)blockDim.x + threadIdx.x;
    if (t >= n) return;
    uint32_t i = list ? list[t] : (uint32_t)t;
    uint32_t e = e32[i];
    if (e & LIT) return;
    e = jump(e32, e);
    e32[i] = e;
    if (!(e & LIT)) next[atomicAdd(nnext, 1u)] = i;
}

__global__ void k_narrow(const uint32_t* e32, uint64_t n, uint8_t* out) {
    uint64_t i = blockIdx.x * (uint64_t)blockDim.x + threadIdx.x;
    if (i < n) out[i] = (uint8_t)e32[i];
}

struct Gpu {
    uint64_t size, padded, nchunks, njobs_max, pool, expect_size;
    unsigned maxcand;
    uint8_t *h_in, *h_out, *d_in, *d_final;
    uint64_t *h_first, *d_first, *d_off, *d_poff, *d_pdst, *d_cand;
    uint32_t *d_cap, *d_plen, *d_e32, *d_work[2];
    unsigned *d_ncand, *d_nwork;
    int *d_order, *d_tfirst;
    gi::Start *d_jobs, *d_targets;
    uint16_t* d_out16;
    gi::Result* d_res;
    long long* d_cycles;
    Probe* d_probes;
    gi::Start* d_probed;
    gi::StoredCopy* d_stored;
    unsigned* d_nstored;
    unsigned maxstored;
};

static void gpu_alloc(Gpu& g, const std::vector<uint8_t>& in, size_t size, size_t expect_size, uint64_t chunk_bits) {
    g.size = size;
    g.padded = in.size();
    g.expect_size = expect_size;
    g.nchunks = (size * 8 + chunk_bits - 1) / chunk_bits;
    g.njobs_max = 4 * g.nchunks + 4096;  // starts, probes (one per 4 chunks at most), continuations
    g.pool = 4 * (uint64_t)expect_size + g.njobs_max * SLACK;
    g.maxcand = (unsigned)std::min<uint64_t>(size * 8 / 16, 0xffffffffull);  // room for 1 survivor in 16 positions
    CK(cudaMallocHost(&g.h_in, g.padded));
    CK(cudaMallocHost(&g.h_out, expect_size));
    CK(cudaMallocHost(&g.h_first, g.nchunks * 8));
    memcpy(g.h_in, in.data(), g.padded);
    CK(cudaMalloc(&g.d_in, g.padded));
    CK(cudaMalloc(&g.d_final, expect_size));
    CK(cudaMalloc(&g.d_first, g.nchunks * 8));
    for (uint64_t** p : {&g.d_off, &g.d_poff, &g.d_pdst}) CK(cudaMalloc(p, g.njobs_max * 8));
    CK(cudaMalloc(&g.d_probes, g.njobs_max * sizeof(Probe)));
    CK(cudaMalloc(&g.d_probed, g.njobs_max * sizeof(gi::Start)));
    g.maxstored = (unsigned)(size / 1024 + 65536);  // stored blocks hold up to 64 KB each
    CK(cudaMalloc(&g.d_stored, g.maxstored * sizeof(gi::StoredCopy)));
    CK(cudaMalloc(&g.d_nstored, sizeof(unsigned)));
    CK(cudaMalloc(&g.d_cand, (uint64_t)g.maxcand * 8));
    CK(cudaMalloc(&g.d_cap, g.njobs_max * 4));
    CK(cudaMalloc(&g.d_plen, g.njobs_max * 4));
    CK(cudaMalloc(&g.d_order, g.njobs_max * 4));
    CK(cudaMalloc(&g.d_tfirst, g.njobs_max * 4));
    CK(cudaMalloc(&g.d_jobs, g.njobs_max * sizeof(gi::Start)));
    CK(cudaMalloc(&g.d_targets, g.njobs_max * sizeof(gi::Start)));
    CK(cudaMalloc(&g.d_e32, expect_size * 4));
    for (auto& w : g.d_work) CK(cudaMalloc(&w, expect_size * 4));
    CK(cudaMalloc(&g.d_ncand, sizeof(unsigned)));
    CK(cudaMalloc(&g.d_nwork, 2 * sizeof(unsigned)));
    CK(cudaMalloc(&g.d_out16, g.pool * 2));
    CK(cudaMalloc(&g.d_res, g.njobs_max * sizeof(gi::Result)));
    CK(cudaMalloc(&g.d_cycles, g.njobs_max * 8));
}

static bool gpu_speculative(Gpu& g, const std::vector<uint8_t>& expect, uint64_t chunk_bits) {
    uint64_t nbits = g.size * 8;
    double ratio = (double)g.expect_size / (double)std::max<uint64_t>(g.size, 1);
    cudaEvent_t ev[6];
    for (auto& e : ev) CK(cudaEventCreate(&e));
    auto t0 = clk::now();
    CK(cudaEventRecord(ev[0]));
    CK(cudaMemcpy(g.d_in, g.h_in, g.padded, cudaMemcpyHostToDevice));
    CK(cudaEventRecord(ev[1]));
    CK(cudaMemsetAsync(g.d_first, 0xff, g.nchunks * 8));
    CK(cudaMemsetAsync(g.d_ncand, 0, sizeof(unsigned)));
    uint64_t nwords = (g.size + 3) / 4;
    k_prefilter<<<(unsigned)((nwords + 255) / 256), 256>>>((const uint32_t*)g.d_in, nwords, nbits, g.d_cand, g.d_ncand,
                                                         g.maxcand);
    CK(cudaGetLastError());
    unsigned ncand = 0;
    CK(cudaMemcpy(&ncand, g.d_ncand, sizeof(unsigned), cudaMemcpyDeviceToHost));
    if (ncand > g.maxcand) {
        fprintf(stderr, "  too many candidate block starts (%u)\n", ncand);
        return false;
    }
    if (getenv("GI_DEBUG")) printf("   candidates after the cheap test: %u\n", ncand);
    k_verify<<<(ncand + 255) / 256, 256>>>(g.d_in, g.size, g.d_cand, ncand, chunk_bits, (unsigned long long*)g.d_first);
    CK(cudaGetLastError());
    CK(cudaMemcpy(g.h_first, g.d_first, g.nchunks * 8, cudaMemcpyDeviceToHost));
    std::vector<gi::Start> starts = block_starts(g.h_first, g.nchunks);
    std::vector<Probe> pts = probe_points(starts, nbits, 4 * chunk_bits);
    if (pts.size() > g.njobs_max) pts.resize(g.njobs_max);
    std::vector<gi::Start> probed(pts.size());
    float probe_ms = 0;
    if (!pts.empty()) {
        cudaEvent_t p0, p1;
        CK(cudaEventCreate(&p0));
        CK(cudaEventCreate(&p1));
        CK(cudaMemcpy(g.d_probes, pts.data(), pts.size() * sizeof(Probe), cudaMemcpyHostToDevice));
        CK(cudaEventRecord(p0));
        k_probe<<<(unsigned)((pts.size() + 7) / 8), DECODE_BLOCK, 8 * LUT_STRIDE * 2>>>(g.d_in, g.size, g.d_probes,
                                                                                       (int)pts.size(), g.d_probed);
        CK(cudaGetLastError());
        CK(cudaEventRecord(p1));
        CK(cudaMemcpy(probed.data(), g.d_probed, pts.size() * sizeof(gi::Start), cudaMemcpyDeviceToHost));
        CK(cudaEventElapsedTime(&probe_ms, p0, p1));
        CK(cudaEventDestroy(p0));
        CK(cudaEventDestroy(p1));
    }
    starts = merge_starts(std::move(starts), probed, nbits);
    CK(cudaEventRecord(ev[2]));
    if (getenv("GI_DEBUG")) printf("   probe kernel: %.1f ms for %zu probes\n", probe_ms, pts.size());
    Jobs j = make_jobs(starts, nbits, ratio, g.pool);
    size_t n0 = j.start.size();
    // one GPU thread decodes about 9 MB/s: a span this long would run into Windows' 2-second
    // limit on a kernel
    uint64_t longest = 0;
    for (size_t k = 0; k < n0; k++) longest = std::max(longest, (k + 1 < n0 ? starts[k + 1].bit : nbits) - starts[k].bit);
    if (longest / 8 > (4u << 20) || n0 > g.njobs_max / 2 || j.used > g.pool) {
        printf("GPU speculative:            skipped: a stretch of %llu KB could not be split\n",
               (unsigned long long)(longest / 8192));
        return true;
    }
    CK(cudaMemcpyAsync(g.d_targets, starts.data(), n0 * sizeof(gi::Start), cudaMemcpyHostToDevice));
    std::vector<int> order(n0);
    std::iota(order.begin(), order.end(), 0);
    std::sort(order.begin(), order.end(), [&](int a, int b) { return j.cap[a] > j.cap[b]; });
    CK(cudaMemcpyAsync(g.d_order, order.data(), n0 * 4, cudaMemcpyHostToDevice));
    CK(cudaMemsetAsync(g.d_nstored, 0, sizeof(unsigned)));
    gi::StoredCopies stored{g.d_stored, g.d_nstored, g.maxstored};
    Pieces s;
    int rounds = decode_all(j, s, [&](size_t a, size_t b) {
        if (b > g.njobs_max) {
            fprintf(stderr, "  too many jobs\n");
            exit(1);
        }
        size_t n = b - a;
        CK(cudaMemcpyAsync(g.d_jobs + a, j.start.data() + a, n * sizeof(gi::Start), cudaMemcpyHostToDevice));
        CK(cudaMemcpyAsync(g.d_tfirst + a, j.tfirst.data() + a, n * 4, cudaMemcpyHostToDevice));
        CK(cudaMemcpyAsync(g.d_off + a, j.off.data() + a, n * 8, cudaMemcpyHostToDevice));
        CK(cudaMemcpyAsync(g.d_cap + a, j.cap.data() + a, n * 4, cudaMemcpyHostToDevice));
        k_decode<<<(unsigned)((n + 7) / 8), DECODE_BLOCK, 8 * LUT_STRIDE * 2>>>(
            g.d_in, g.size, g.d_jobs, g.d_tfirst, g.d_off, g.d_cap, g.d_targets, (int)n0, a == 0 ? g.d_order : nullptr,
            (int)a, (int)n, g.d_out16, g.d_res, g.d_cycles, stored);
        CK(cudaGetLastError());
        CK(cudaMemcpy(j.res.data() + a, g.d_res + a, n * sizeof(gi::Result), cudaMemcpyDeviceToHost));
    });
    CK(cudaEventRecord(ev[3]));
    if (rounds < 0 || s.total != g.expect_size) {
        fprintf(stderr, "  GPU output is %llu bytes, expected %llu\n", (unsigned long long)s.total,
                (unsigned long long)g.expect_size);
        return false;
    }
    unsigned nstored = 0;
    CK(cudaMemcpy(&nstored, g.d_nstored, sizeof(unsigned), cudaMemcpyDeviceToHost));
    nstored = std::min(nstored, g.maxstored);
    if (nstored) k_stored<<<nstored, 256>>>(g.d_stored, nstored);
    int np = (int)s.off.size();
    CK(cudaMemcpyAsync(g.d_poff, s.off.data(), np * 8, cudaMemcpyHostToDevice));
    CK(cudaMemcpyAsync(g.d_pdst, s.dst.data(), np * 8, cudaMemcpyHostToDevice));
    CK(cudaMemcpyAsync(g.d_plen, s.len.data(), np * 4, cudaMemcpyHostToDevice));
    k_absolute<<<np, 256>>>(g.d_out16, g.d_poff, g.d_plen, g.d_pdst, g.d_e32);
    CK(cudaGetLastError());
    int jumps = 0;
    const uint32_t* list = nullptr;  // first round: every entry
    for (uint64_t pending = g.expect_size; pending; jumps++) {
        unsigned* cnt = g.d_nwork + (jumps & 1);
        uint32_t* next = g.d_work[jumps & 1];
        CK(cudaMemsetAsync(cnt, 0, sizeof(unsigned)));
        k_jump<<<(unsigned)((pending + 255) / 256), 256>>>(g.d_e32, pending, list, next, cnt);
        CK(cudaGetLastError());
        unsigned left = 0;
        CK(cudaMemcpy(&left, cnt, sizeof(unsigned), cudaMemcpyDeviceToHost));
        pending = left;
        list = next;
    }
    k_narrow<<<(unsigned)((g.expect_size + 255) / 256), 256>>>(g.d_e32, g.expect_size, g.d_final);
    CK(cudaGetLastError());
    CK(cudaEventRecord(ev[4]));
    CK(cudaMemcpy(g.h_out, g.d_final, g.expect_size, cudaMemcpyDeviceToHost));
    CK(cudaEventRecord(ev[5]));
    CK(cudaEventSynchronize(ev[5]));
    double wall = since(t0);
    float ms[5];
    for (int i = 0; i < 5; i++) CK(cudaEventElapsedTime(&ms[i], ev[i], ev[i + 1]));
    bool good = memcmp(g.h_out, expect.data(), g.expect_size) == 0;
    printf("GPU speculative:            %6.3f s = %5.2f GB/s  [h2d %.1f · find+probe %.1f · decode %.1f (%d rounds)"
           " · resolve %.1f (%d jumps) · d2h %.1f ms]  starts %zu (%zu probes), jobs %zu, pieces %d  %s\n",
           wall, g.expect_size / wall / 1e9, ms[0], ms[1], ms[2], rounds, ms[3], jumps, ms[4], n0, pts.size(),
           j.start.size(), np, good ? "OK" : "WRONG OUTPUT");
    for (auto& e : ev) CK(cudaEventDestroy(e));
    return good;
}

int main(int argc, char** argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: gpuinflate <raw-deflate> <expected> [chunk_kb=4]\n");
        return 2;
    }
    size_t size, expect_size;
    std::vector<uint8_t> in = read_file(argv[1], size), expect = read_file(argv[2], expect_size);
    uint64_t chunk_bits = (uint64_t)(argc > 3 ? atoi(argv[3]) : 4) * 1024 * 8;
    printf("%s: %zu bytes compressed, %zu expected, chunks of %llu KB\n", argv[1], size, expect_size,
           (unsigned long long)(chunk_bits / 8192));

    // 1. our decoder, one CPU thread, from the start: checks the decoder itself
    {
        std::vector<uint16_t> out16(expect_size + 2 * gi::MAX_SYMBOL_OUT);
        std::vector<uint8_t> out(expect_size);
        CpuDecoder* d = new CpuDecoder;
        auto t = clk::now();
        gi::Result r = d->dec.run(in.data(), size, {0, 0, gi::AT_BLOCK, -1}, nullptr, 0, out16.data(),
                                  (uint32_t)out16.size(), true);
        for (uint32_t i = 0; i < r.out_len && i < expect_size; i++)
            out[i] = out16[i] <= gi::REF ? (uint8_t)out16[i] : out[i - (out16[i] - gi::REF)];
        double s = since(t);
        bool good = r.status == gi::OK && r.out_len == expect_size && memcmp(out.data(), expect.data(), expect_size) == 0;
        printf("CPU sequential,  1 thread:  %6.3f s = %5.2f GB/s  blocks %u  %s\n", s, expect_size / s / 1e9, r.blocks,
               good ? "OK" : "WRONG OUTPUT");
        delete d;
        if (!good) return 1;
    }
    // 2. the speculative algorithm on CPU threads
    if (!cpu_speculative(in, size, expect, expect_size, chunk_bits, (int)std::thread::hardware_concurrency())) return 1;
    // 3. the same on the GPU, three times (the first run pays for start-up costs)
    CK(cudaSetDevice(0));
    CK(cudaDeviceSetLimit(cudaLimitStackSize, 16384));
    CK(cudaFuncSetAttribute(k_decode, cudaFuncAttributeMaxDynamicSharedMemorySize, 8 * LUT_STRIDE * 2));
    CK(cudaFuncSetAttribute(k_probe, cudaFuncAttributeMaxDynamicSharedMemorySize, 8 * LUT_STRIDE * 2));
    cudaDeviceProp prop;
    CK(cudaGetDeviceProperties(&prop, 0));
    printf("GPU: %s, %d SMs\n", prop.name, prop.multiProcessorCount);
    Gpu g;
    gpu_alloc(g, in, size, expect_size, chunk_bits);
    for (int run = 0; run < 3; run++)
        if (!gpu_speculative(g, expect, chunk_bits)) return 1;
    return 0;
}
