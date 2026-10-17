// gpuinflate.cu - one raw Deflate stream, decompressed in parallel on the GPU, in segments.
//
// Each segment is a stretch of compressed input that starts at an exactly known decoder state
// (bit position, block type and tables, BFINAL) and is decoded speculatively:
//   1. find plausible dynamic-block starts in every 4 KB chunk (a cheap test on every bit
//      position, then zlib's full header checks on the survivors);
//   2. probe inside long stretches without one (fixed-Huffman blocks, huge blocks): decode
//      from 4 KB before a point and take the first symbol boundary after it;
//   3. decode from every start at once, each decoder until it lands exactly on a later start
//      with the same tables; decoders write bytes and back-references, not copies. A decoder
//      whose buffer fills stops cleanly and a new one continues from there in the next round;
//   4. stitch the pieces that connect from the segment's start; the chain ends at the stream's
//      last block, at the segment's stop bit, or where the segment's output budget runs out,
//      and wherever it ends, the state there is exact: the next segment starts from it;
//   5. turn every back-reference into an absolute position (the 32 KB before the segment are
//      kept on the GPU) and resolve them all by pointer jumping.
// The output goes to the caller's sink from a thread of its own while the next segment runs.
#define GI_BUILD
#include "gpuinflate.h"
#include "inflate.cuh"

#include <cuda_runtime.h>

#include <algorithm>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <mutex>
#include <numeric>
#include <queue>
#include <string>
#include <thread>
#include <vector>

namespace {

struct CudaFail {
    std::string msg;
};

#define CK(x)                                                                                    \
    do {                                                                                         \
        cudaError_t e_ = (x);                                                                    \
        if (e_ != cudaSuccess) throw CudaFail{std::string(#x) + ": " + cudaGetErrorString(e_)}; \
    } while (0)

using clk = std::chrono::steady_clock;
double since(clk::time_point t) { return std::chrono::duration<double>(clk::now() - t).count(); }

constexpr uint64_t SLACK = 8192;                 // output symbols every job gets beyond its share
constexpr uint32_t LIT = 0x80000000u;            // resolved entry: a byte (low 8 bits); else the position it copies
constexpr int HOPS = 8;                          // pointer-jumping steps per entry per round
constexpr int MAX_ROUNDS = 64;                   // decode rounds per segment before cutting it short
constexpr int MAX_JUMPS = 64;                    // pointer-jumping rounds (about log8 of the deepest chain)
constexpr uint64_t CHUNK_BITS = 4096 * 8;        // the block finder keeps one start per chunk
constexpr uint64_t PROBE_SPACING = 4 * CHUNK_BITS;
constexpr uint64_t PROBE_LEAD = 4096 * 8;        // a probe starts decoding this many bits before its point
constexpr uint32_t JOB_CAP_MAX = 4u << 20;       // symbols one decoder writes per kernel: bounds its time
constexpr uint32_t JOB_CAP_MIN = 4096;
constexpr uint64_t HDR_BYTES = 512;              // a segment's input begins with a copy of its first block's header
constexpr uint64_t HDR_BITS = HDR_BYTES * 8;
constexpr uint64_t MARGIN = 128 << 10;           // input past a segment's stop: a stored block begun before it fits
constexpr uint64_t SEG_IN_MAX = 64ull << 20;
constexpr uint64_t SEG_IN_MIN = 256 << 10;
constexpr uint64_t IN_PAD = 64;                  // zeros after the input: the bit reader and finder read ahead
constexpr uint32_t WINDOW = 32768;
constexpr uint64_t SEG_OUT_AUTO_MAX = 256ull << 20;
constexpr uint64_t SEG_OUT_MIN = 8ull << 20;     // at least 2 * JOB_CAP_MAX
constexpr int OUT_BUFS = 4;                      // pinned buffers the output reaches the sink through
constexpr uint64_t OUT_BUF = 16ull << 20;
constexpr int IN_BUFS = 2;                       // pinned buffers the input goes up through
constexpr uint64_t IN_BUF = 8ull << 20;

// ---------------------------------------------------------------- planning (host)

// points to probe inside stretches longer than 2 * spacing without a block start, each with
// the tables the stretch begins with
struct Probe {
    uint64_t header, point;
    uint8_t mode;
};

// the segment's start first, then the block starts found, one per chunk at most
std::vector<gi::Start> block_starts(const uint64_t* first, uint64_t nchunks, const gi::Start& s0) {
    std::vector<gi::Start> v{s0};
    for (uint64_t c = 0; c < nchunks; c++)
        if (first[c] != UINT64_MAX && first[c] > v.back().bit) v.push_back({first[c], first[c], gi::AT_BLOCK, -1});
    return v;
}

std::vector<Probe> probe_points(const std::vector<gi::Start>& s, uint64_t end, uint64_t spacing) {
    std::vector<Probe> pts;
    for (size_t k = 0; k < s.size(); k++) {
        uint64_t e = k + 1 < s.size() ? s[k + 1].bit : end;
        if (e - s[k].bit < 2 * spacing) continue;
        uint64_t header = s[k].mode == gi::AT_BLOCK ? s[k].bit : s[k].header;
        for (uint64_t b = s[k].bit + spacing; b + spacing / 2 < e; b += spacing) pts.push_back({header, b, s[k].mode});
    }
    return pts;
}

std::vector<gi::Start> merge_starts(std::vector<gi::Start> s, const std::vector<gi::Start>& probed, uint64_t lo,
                                    uint64_t end) {
    for (const gi::Start& e : probed)
        if (e.bit > lo && e.bit < end) s.push_back(e);
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
    size_t max_jobs = 0;

    void add(const gi::Start& s, int tf, uint32_t c) {
        start.push_back(s);
        tfirst.push_back(tf);
        off.push_back(used);
        cap.push_back(c);
        used += c;
        res.push_back({});
        cont.push_back(-1);
    }
};

// one job per start; buffers sized from the stream's ratio, twice over, scaled to fit half the pool
Jobs make_jobs(std::vector<gi::Start> targets, uint64_t end, double ratio, uint64_t pool, size_t max_jobs) {
    Jobs j;
    j.pool = pool;
    j.max_jobs = max_jobs;
    j.targets = std::move(targets);
    size_t n = j.targets.size();
    std::vector<double> want(n);
    double sum = 0;
    for (size_t k = 0; k < n; k++) {
        uint64_t span = (k + 1 < n ? j.targets[k + 1].bit : end) - j.targets[k].bit;
        want[k] = std::min<double>(2 * ratio * (double)(span / 8) + SLACK, JOB_CAP_MAX);
        sum += want[k];
    }
    double f = sum > pool / 2 ? (double)(pool / 2) / sum : 1.0;
    for (size_t k = 0; k < n; k++)
        j.add(j.targets[k], (int)k + 1, (uint32_t)std::max<double>(JOB_CAP_MIN, want[k] * f));
    return j;
}

// A continuation for job k; others leave `reserve` symbols of the pool for the chain's own
bool add_continuation(Jobs& j, size_t k, uint64_t reserve) {
    if (j.cont[k] >= 0 || j.start.size() >= j.max_jobs) return false;
    // it lands only on targets after its start: one at its start may be the job it continues
    const gi::Start& r = j.res[k].resume;
    int tf = (int)(std::upper_bound(j.targets.begin(), j.targets.end(), r.bit,
                                    [](uint64_t b, const gi::Start& a) { return b < a.bit; }) -
                   j.targets.begin());
    uint64_t left = j.pool - j.used;
    if (left < reserve + JOB_CAP_MIN) return false;
    uint64_t c = std::min<uint64_t>({std::max<uint64_t>(4ull * j.cap[k], 1u << 20), JOB_CAP_MAX, left - reserve});
    j.cont[k] = (int)j.start.size();
    j.add(r, tf, (uint32_t)c);
    return true;
}

struct Pieces {
    std::vector<uint64_t> off;  // outputs of the pieces that chain from the segment's start
    std::vector<uint32_t> len;
    std::vector<uint64_t> dst;  // where each piece goes in the segment's output
    uint64_t total = 0;
};

enum Walk { DONE, NEED, CUT, FAIL };

struct Chain {
    Walk walk = FAIL;
    Pieces s;
    gi::Start next{};     // CUT (and NEED): the exact state where the chain stops
    uint64_t end_bit = 0; // DONE: just past the stream's last block
    int need = -1;        // NEED: the job whose continuation is missing
    int status = 0;       // FAIL: the decoder's status
    uint64_t fail_bit = 0;
};

gi::Start with_final(gi::Start s, int final) {
    if (s.mode != gi::AT_BLOCK && s.final < 0) s.final = (int8_t)final;
    return s;
}

// Follow the pieces from job 0: each landing names the job that goes on from there, each
// overflow its continuation. The chain's output stops before out_limit is passed.
Chain stitch(const Jobs& j, uint64_t out_limit, bool debug) {
    Chain c;
    int k = 0;
    int final = j.start[0].final >= 0 ? j.start[0].final : 0;  // BFINAL of the block the next piece starts in
    for (size_t steps = 0; steps <= j.start.size(); steps++) {
        const gi::Result& r = j.res[k];
        const gi::Start& st = j.start[k];
        // a piece that starts inside the last block, unaware of it, ends at its end-of-block code
        bool last = st.mode != gi::AT_BLOCK && st.final < 0 && final == 1 && r.eob_bit != UINT64_MAX;
        if (debug && (r.status != gi::OK || r.landed < 0 || last))
            fprintf(stderr, "    job #%d at bit %llu mode %d: status %d, %u symbols, landed %d, end_final %d\n", k,
                    (unsigned long long)st.bit, st.mode, r.status, r.out_len, r.landed, r.end_final);
        uint32_t len = last ? r.eob_out : r.out_len;
        if (!last && r.status != gi::OK && r.status != gi::ERR_OVERFLOW && r.status != gi::STOPPED) {
            c.walk = FAIL;
            c.status = r.status;
            c.fail_bit = r.end_bit;
            return c;
        }
        if (c.s.total + len > out_limit) {
            c.walk = CUT;
            c.next = with_final(st, final);
            return c;
        }
        c.s.off.push_back(j.off[k]);
        c.s.len.push_back(len);
        c.s.dst.push_back(c.s.total);
        c.s.total += len;
        if (r.end_final >= 0) final = r.end_final;
        if (last) {
            c.walk = DONE;
            c.end_bit = r.eob_bit;
            return c;
        }
        if (r.status == gi::ERR_OVERFLOW || r.status == gi::STOPPED) {
            c.next = with_final(r.resume, final);
            if (r.status == gi::STOPPED) {
                c.walk = CUT;
                return c;
            }
            if (j.cont[k] < 0 || j.res[j.cont[k]].status < 0) {
                c.walk = NEED;
                c.need = k;
                return c;
            }
            k = j.cont[k];
            continue;
        }
        if (r.landed < 0) {
            c.walk = DONE;
            c.end_bit = r.end_bit;
            return c;
        }
        k = j.tfirst[k] + r.landed;
    }
    c.walk = FAIL;
    c.status = -1;
    return c;
}

// Decode every job, adding continuations until the chain ends. decode(a, b) decodes jobs
// a..b-1 in parallel. Out of rounds, jobs or buffer: the chain is cut where it got to.
Chain decode_all(Jobs& j, uint64_t out_limit, bool debug, int& rounds, const std::function<void(size_t, size_t)>& decode) {
    size_t a = 0;
    Chain c;
    for (int round = 1; round <= MAX_ROUNDS; round++) {
        for (size_t k = a; k < j.start.size(); k++) j.res[k].status = -1;  // not decoded yet
        decode(a, j.start.size());
        rounds++;
        c = stitch(j, out_limit, debug);
        if (c.walk != NEED) return c;
        size_t first = j.start.size();
        if (!add_continuation(j, (size_t)c.need, 0)) {
            c.walk = CUT;
            return c;
        }
        // continue every other job that overflowed in this round too: the chain may pass through them
        for (size_t k = a; k < first; k++)
            if (j.res[k].status == gi::ERR_OVERFLOW && j.cont[k] < 0) add_continuation(j, k, 2ull * JOB_CAP_MAX);
        a = first;
    }
    c.walk = CUT;
    return c;
}

// ---------------------------------------------------------------- GPU kernels

// Finder, pass 1: every bit position in (bit_lo, bit_hi) gets the same cheap test (one thread
// per 32-bit word, 32 offsets each); survivors are appended to a list
__global__ void k_prefilter(const uint32_t* w, uint64_t word_lo, uint64_t nwords, uint64_t bit_lo, uint64_t bit_hi,
                            uint64_t* cand, unsigned* ncand, unsigned maxcand) {
    uint64_t t = blockIdx.x * (uint64_t)blockDim.x + threadIdx.x;
    if (t >= nwords) return;
    uint64_t word = word_lo + t;
    uint64_t a = __ldg(w + word), b = __ldg(w + word + 1), c = __ldg(w + word + 2), d = __ldg(w + word + 3);
    uint64_t lo = a | b << 32, hi = c | d << 32;
    for (int o = 0; o < 32; o++) {
        uint64_t bit = word * 32 + o;
        if (bit <= bit_lo || bit >= bit_hi) continue;
        uint64_t v1 = o ? (lo >> o) | (hi << (64 - o)) : lo;
        uint64_t v2 = hi >> o;
        if (gi::header_prefilter128(v1, v2)) {
            unsigned i = atomicAdd(ncand, 1u);
            if (i < maxcand) cand[i] = bit;
        }
    }
}

// Finder, pass 2: the full header check for the survivors; each chunk keeps its first hit
__global__ void k_verify(const uint8_t* in, uint64_t size, const uint64_t* cand, unsigned ncand, uint64_t base,
                         unsigned long long* first) {
    unsigned i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= ncand) return;
    uint64_t bit = cand[i];
    uint64_t c = (bit - base) / CHUNK_BITS;
    if (bit >= first[c]) return;
    if (gi::plausible_dynamic_header(in, size, bit)) atomicMin(&first[c], (unsigned long long)bit);
}

constexpr int LUT_STRIDE = gi::LUT_U16 + 2;  // odd word count per decoder: fewer shared-memory bank conflicts
constexpr int DECODE_BLOCK = 256;            // threads per block: 8 warps, 8 decoders
constexpr int DECODE_SHMEM = 8 * LUT_STRIDE * 2;
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
    Probe p = pts[w];
    probed[w] = dec.probe(in, size, p.mode, p.header, p.point - PROBE_LEAD, p.point);
}

// the stored blocks the decoders skipped, one block of threads per stored block
__global__ void k_stored(const gi::StoredCopy* list, unsigned n) {
    if (blockIdx.x >= n) return;
    gi::StoredCopy c = list[blockIdx.x];
    for (uint32_t i = threadIdx.x; i < c.len; i += blockDim.x) c.dst[i] = c.src[i];
}

// Warps take jobs a.. (through `order` for the first round: longest first)
__global__ void __launch_bounds__(DECODE_BLOCK, GI_DECODE_BLOCKS_PER_SM)
k_decode(const uint8_t* in, uint64_t size, const gi::Start* jobs, const int* tfirst, const uint64_t* off,
         const uint32_t* cap, const gi::Start* targets, int ntargets, const int* order, int a, int n, uint16_t* out,
         gi::Result* res, bool stream_start, uint64_t stop, gi::StoredCopies stored) {
    extern __shared__ uint16_t luts[];
    int w = (blockIdx.x * blockDim.x + threadIdx.x) >> 5;
    if (w >= n || (threadIdx.x & 31)) return;
    int k = order ? order[w] : a + w;
    gi::Decoder dec;
    dec.attach(luts + (threadIdx.x >> 5) * LUT_STRIDE);
    res[k] = dec.run(in, size, jobs[k], targets + tfirst[k], ntargets - tfirst[k], out + off[k], cap[k],
                     stream_start && k == 0, stop, &stored);
}

// the 32 KB before the segment, as resolved entries
__global__ void k_window(uint32_t* e32, const uint8_t* window, uint32_t n) {
    uint32_t i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) e32[i] = LIT | window[i];
}

// every output position as a byte or the absolute position it copies (one block per piece); a
// reference before the stream's start is invalid data
__global__ void k_absolute(const uint16_t* out16, const uint64_t* poff, const uint32_t* plen, const uint64_t* pdst,
                           uint32_t* e32, int* bad) {
    int q = blockIdx.x;
    const uint16_t* src = out16 + poff[q];
    uint64_t dst = pdst[q];
    for (uint32_t i = threadIdx.x; i < plen[q]; i += blockDim.x) {
        uint16_t v = src[i];
        uint64_t pos = dst + i;
        uint32_t e = LIT | v;
        if (v > gi::REF) {
            uint64_t d = v - gi::REF;
            if (d <= pos) {
                e = (uint32_t)(pos - d);
            } else {
                e = LIT;
                *bad = 1;
            }
        }
        e32[pos] = e;
    }
}

// follow e's chain up to HOPS steps in the entries array
__device__ __forceinline__ uint32_t jump(const uint32_t* e32, uint32_t e) {
    for (int h = 0; h < HOPS && !(e & LIT); h++) e = e32[e];
    return e;
}

// One round of pointer jumping over the entries in `list` (all n when list is null): each
// follows its chain up to HOPS steps, in place; the ones still unresolved go to `next` (when
// more than cap are left, the next round goes over all entries again)
__global__ void k_jump(uint32_t* e32, uint64_t n, const uint32_t* list, uint32_t* next, unsigned* nnext, uint64_t cap) {
    uint64_t t = blockIdx.x * (uint64_t)blockDim.x + threadIdx.x;
    if (t >= n) return;
    uint32_t i = list ? list[t] : (uint32_t)t;
    uint32_t e = e32[i];
    if (e & LIT) return;
    e = jump(e32, e);
    e32[i] = e;
    if (!(e & LIT)) {
        unsigned q = atomicAdd(nnext, 1u);
        if (q < cap) next[q] = i;
    }
}

__global__ void k_narrow(const uint32_t* e32, uint64_t n, uint8_t* out) {
    uint64_t i = blockIdx.x * (uint64_t)blockDim.x + threadIdx.x;
    if (i < n) out[i] = (uint8_t)e32[i];
}

unsigned blocks_for(uint64_t n, unsigned per) { return (unsigned)std::max<uint64_t>(1, (n + per - 1) / per); }

// ---------------------------------------------------------------- the sink's thread

// Hands the output buffers to the sink in order, on a thread of its own, so the GPU goes on
// while the output is written
class Writer {
  public:
    Writer(gi_sink sink, void* user, uint8_t* const* bufs) : sink_(sink), user_(user), bufs_(bufs) {
        th_ = std::thread([this] { loop(); });
    }
    ~Writer() { finish(); }

    void wait_free(int b) {
        std::unique_lock<std::mutex> l(m_);
        cv_.wait(l, [&] { return !busy_[b]; });
    }
    void push(int b, size_t len) {
        std::lock_guard<std::mutex> l(m_);
        busy_[b] = true;
        q_.push({b, len});
        cv_.notify_all();
    }
    bool aborted() {
        std::lock_guard<std::mutex> l(m_);
        return aborted_;
    }
    // waits until everything is written; true if the sink stopped it
    bool finish() {
        {
            std::lock_guard<std::mutex> l(m_);
            done_ = true;
            cv_.notify_all();
        }
        if (th_.joinable()) th_.join();
        return aborted_;
    }

  private:
    void loop() {
        std::unique_lock<std::mutex> l(m_);
        for (;;) {
            cv_.wait(l, [&] { return done_ || !q_.empty(); });
            if (q_.empty()) return;
            auto [b, len] = q_.front();
            q_.pop();
            bool skip = aborted_;
            l.unlock();
            int r = !skip && sink_ ? sink_(user_, bufs_[b], len) : 0;
            l.lock();
            if (r) aborted_ = true;
            busy_[b] = false;
            cv_.notify_all();
        }
    }

    gi_sink sink_;
    void* user_;
    uint8_t* const* bufs_;
    std::thread th_;
    std::mutex m_;
    std::condition_variable cv_;
    std::queue<std::pair<int, size_t>> q_;
    bool busy_[OUT_BUFS] = {};
    bool done_ = false, aborted_ = false;
};

}  // namespace

// ---------------------------------------------------------------- context

struct gi_ctx {
    int device = 0;
    std::string name, err;
    bool debug = false;
    uint64_t seg_out_max = 0, pool = 0, njobs_max = 0, nchunks_max = 0, work_cap = 0, in_cap = 0;
    unsigned maxcand = 0, maxstored = 0;
    uint64_t forced_seg_in = 0;
    cudaStream_t st = nullptr;
    cudaEvent_t ev[6] = {};

    uint8_t *d_in = nullptr, *d_final = nullptr, *d_window = nullptr;
    uint64_t *d_first = nullptr, *d_cand = nullptr, *d_off = nullptr, *d_poff = nullptr, *d_pdst = nullptr;
    unsigned *d_ncand = nullptr, *d_nstored = nullptr, *d_nwork = nullptr;
    int *d_tfirst = nullptr, *d_order = nullptr, *d_bad = nullptr;
    uint32_t *d_cap = nullptr, *d_plen = nullptr, *d_e32 = nullptr, *d_work[2] = {};
    Probe* d_probes = nullptr;
    gi::Start *d_probed = nullptr, *d_jobs = nullptr, *d_targets = nullptr;
    gi::Result* d_res = nullptr;
    uint16_t* d_out16 = nullptr;
    gi::StoredCopy* d_stored = nullptr;

    uint8_t* h_in[IN_BUFS] = {};
    uint64_t* h_first = nullptr;
    uint8_t* h_out[OUT_BUFS] = {};
    cudaEvent_t in_done[IN_BUFS] = {};

    std::vector<void*> dev_allocs, host_allocs;

    template <class T>
    void dalloc(T*& p, uint64_t count) {
        void* v = nullptr;
        CK(cudaMalloc(&v, std::max<uint64_t>(count, 1) * sizeof(T)));
        dev_allocs.push_back(v);
        p = (T*)v;
    }
    template <class T>
    void halloc(T*& p, uint64_t count) {
        void* v = nullptr;
        CK(cudaMallocHost(&v, std::max<uint64_t>(count, 1) * sizeof(T)));
        host_allocs.push_back(v);
        p = (T*)v;
    }
    void release() {
        for (void* p : dev_allocs) cudaFree(p);
        for (void* p : host_allocs) cudaFreeHost(p);
        dev_allocs.clear();
        host_allocs.clear();
        for (auto& e : ev)
            if (e) cudaEventDestroy(e), e = nullptr;
        for (auto& e : in_done)
            if (e) cudaEventDestroy(e), e = nullptr;
        if (st) cudaStreamDestroy(st), st = nullptr;
    }
};

namespace {

void setup(gi_ctx& g, const gi_config& cfg) {
    g.device = cfg.device;
    g.debug = getenv("GI_DEBUG") != nullptr;
    auto t0 = clk::now();
    CK(cudaSetDevice(g.device));
    CK(cudaFree(nullptr));  // creates the context
    double t_ctx = since(t0);
    cudaDeviceProp prop;
    CK(cudaGetDeviceProperties(&prop, g.device));
    g.name = prop.name;
    CK(cudaStreamCreateWithFlags(&g.st, cudaStreamNonBlocking));
    for (auto& e : g.ev) CK(cudaEventCreate(&e));
    for (auto& e : g.in_done) CK(cudaEventCreateWithFlags(&e, cudaEventDisableTiming));
    CK(cudaFuncSetAttribute(k_decode, cudaFuncAttributeMaxDynamicSharedMemorySize, DECODE_SHMEM));
    CK(cudaFuncSetAttribute(k_probe, cudaFuncAttributeMaxDynamicSharedMemorySize, DECODE_SHMEM));

    // a segment's input: the header copy, at most SEG_IN_MAX, the margin, and 3 bytes more
    // because it starts at cur.bit / 8 rounded down to a multiple of 4; then the zeros after it
    // (IN_PAD + 4). Without the 3, a full-size segment (a stream that compresses less than about
    // 2.4:1) starting at an unaligned byte ran its zeros past the buffer: "invalid argument".
    g.in_cap = HDR_BYTES + SEG_IN_MAX + MARGIN + 3 + IN_PAD + 4;
    g.nchunks_max = SEG_IN_MAX * 8 / CHUNK_BITS + 2;
    g.njobs_max = 4 * g.nchunks_max + 8192;
    g.maxcand = (unsigned)(SEG_IN_MAX * 8 / 256);  // room for 1 survivor in 256 positions (real data: 1 in 1100)
    g.maxstored = (unsigned)(SEG_IN_MAX / 1024 + 65536);
    // what does not grow with the output, and then per output byte: the 16-bit pool (2.5
    // symbols), entries (4), work lists (2) and bytes (1)
    uint64_t fixed = g.in_cap + g.nchunks_max * 8 + (uint64_t)g.maxcand * 8 + g.maxstored * sizeof(gi::StoredCopy) +
                     g.njobs_max * (2 * sizeof(gi::Start) + sizeof(gi::Result) + sizeof(Probe) + 4 * 8 + 4 * 4) +
                     (64ull << 20);
    if (cfg.segment_out_mb) {
        g.seg_out_max = (uint64_t)cfg.segment_out_mb << 20;
    } else {
        size_t free_b = 0, total_b = 0;
        CK(cudaMemGetInfo(&free_b, &total_b));
        double usable = 0.6 * (double)free_b - (double)fixed;
        g.seg_out_max = usable > 0 ? (uint64_t)(usable / 12.5) >> 20 << 20 : 0;
        g.seg_out_max = std::min(g.seg_out_max, SEG_OUT_AUTO_MAX);
    }
    if (g.seg_out_max < SEG_OUT_MIN) throw CudaFail{"not enough free GPU memory"};
    g.forced_seg_in = (uint64_t)cfg.segment_in_kb << 10;
    g.pool = std::max<uint64_t>(g.seg_out_max / 2 * 5, 2 * g.njobs_max * JOB_CAP_MIN);
    g.work_cap = (WINDOW + g.seg_out_max) / 4;

    auto t1 = clk::now();
    g.dalloc(g.d_in, g.in_cap);
    g.dalloc(g.d_first, g.nchunks_max);
    g.dalloc(g.d_cand, g.maxcand);
    g.dalloc(g.d_ncand, 1);
    g.dalloc(g.d_probes, g.njobs_max);
    g.dalloc(g.d_probed, g.njobs_max);
    g.dalloc(g.d_jobs, g.njobs_max);
    g.dalloc(g.d_targets, g.njobs_max);
    g.dalloc(g.d_tfirst, g.njobs_max);
    g.dalloc(g.d_order, g.njobs_max);
    g.dalloc(g.d_off, g.njobs_max);
    g.dalloc(g.d_cap, g.njobs_max);
    g.dalloc(g.d_res, g.njobs_max);
    g.dalloc(g.d_poff, g.njobs_max);
    g.dalloc(g.d_pdst, g.njobs_max);
    g.dalloc(g.d_plen, g.njobs_max);
    g.dalloc(g.d_stored, g.maxstored);
    g.dalloc(g.d_nstored, 1);
    g.dalloc(g.d_out16, g.pool);
    g.dalloc(g.d_e32, WINDOW + g.seg_out_max);
    g.dalloc(g.d_work[0], g.work_cap);
    g.dalloc(g.d_work[1], g.work_cap);
    g.dalloc(g.d_nwork, 2);
    g.dalloc(g.d_bad, 1);
    g.dalloc(g.d_final, WINDOW + g.seg_out_max);
    g.dalloc(g.d_window, WINDOW);
    double t_dev = since(t1);
    auto t2 = clk::now();
    for (auto& b : g.h_in) g.halloc(b, IN_BUF);
    g.halloc(g.h_first, g.nchunks_max);
    for (auto& b : g.h_out) g.halloc(b, OUT_BUF);
    if (g.debug)
        fprintf(stderr, "  setup: context %.0f ms, GPU memory %.0f ms, pinned memory %.0f ms (segments up to %llu MB out)\n",
                t_ctx * 1e3, t_dev * 1e3, since(t2) * 1e3, (unsigned long long)(g.seg_out_max >> 20));
}

float elapsed(cudaEvent_t a, cudaEvent_t b) {
    float ms = 0;
    CK(cudaEventElapsedTime(&ms, a, b));
    return ms / 1e3f;
}

int inflate(gi_ctx& g, const uint8_t* in, uint64_t size, uint64_t hint, gi_sink sink, void* user, gi_stats& stats) {
    auto t_start = clk::now();
    CK(cudaSetDevice(g.device));
    cudaStream_t st = g.st;
    Writer writer(sink, user, g.h_out);
    const uint64_t nbits = size * 8;
    gi::Start cur{0, 0, gi::AT_BLOCK, -1};  // host coordinates: bits from the start of `in`
    uint32_t W = 0;                         // bytes of output before this segment kept in d_window
    double ratio = hint ? std::clamp((double)hint / (double)size, 1.0, 1032.0) : 4.0;
    int buf = 0;
    for (;;) {
        if (writer.aborted()) break;
        uint64_t seg_in = g.forced_seg_in
                              ? std::min(g.forced_seg_in, SEG_IN_MAX)
                              : std::clamp<uint64_t>((uint64_t)(0.6 * (double)g.seg_out_max / ratio), SEG_IN_MIN, SEG_IN_MAX);
        // The segment's input on the GPU: a copy of the dynamic header the segment starts
        // under (when it lies before the segment), then the input from just before cur.bit to
        // a margin past the stop bit. Device bit positions count from the copy's start.
        uint64_t lo = (cur.bit / 8) & ~3ull;
        uint64_t stop_h = cur.bit + seg_in * 8;
        bool last = stop_h >= nbits;
        uint64_t hi = last ? size : std::min(size, stop_h / 8 + MARGIN);
        bool hdr_copy = cur.mode == gi::IN_DYNAMIC && cur.header < lo * 8;
        uint64_t dsize = HDR_BYTES + (hi - lo);
        auto dev = [&](uint64_t x) { return x - lo * 8 + HDR_BITS; };
        auto host = [&](uint64_t x) { return x - HDR_BITS + lo * 8; };
        gi::Start s0 = cur;
        s0.bit = dev(cur.bit);
        s0.header = cur.mode == gi::IN_DYNAMIC ? (hdr_copy ? cur.header % 8 : dev(cur.header))
                    : cur.mode == gi::AT_BLOCK ? s0.bit
                                               : UINT64_MAX;
        uint64_t stop_d = last ? UINT64_MAX : dev(stop_h);
        uint64_t end_d = last ? dsize * 8 : stop_d;  // the finder and the probes look in [s0.bit, end_d)

        CK(cudaEventRecord(g.ev[0], st));
        {
            // header copy (or zeros), the input, then zeros: through the pinned buffers, each
            // filled while the one before goes up
            std::vector<uint8_t> head(HDR_BYTES, 0);
            if (hdr_copy) {
                uint64_t hb = cur.header / 8;
                memcpy(head.data(), in + hb, (size_t)std::min<uint64_t>(HDR_BYTES, size - hb));
            }
            CK(cudaMemcpyAsync(g.d_in, head.data(), HDR_BYTES, cudaMemcpyHostToDevice, st));
            CK(cudaStreamSynchronize(st));  // head is pageable and local
            int b = 0;
            for (uint64_t at = lo; at < hi; at += IN_BUF, b = (b + 1) % IN_BUFS) {
                uint64_t n = std::min<uint64_t>(IN_BUF, hi - at);
                CK(cudaEventSynchronize(g.in_done[b]));
                memcpy(g.h_in[b], in + at, (size_t)n);
                CK(cudaMemcpyAsync(g.d_in + HDR_BYTES + (at - lo), g.h_in[b], n, cudaMemcpyHostToDevice, st));
                CK(cudaEventRecord(g.in_done[b], st));
            }
            CK(cudaMemsetAsync(g.d_in + dsize, 0, IN_PAD + 4, st));
        }
        CK(cudaEventRecord(g.ev[1], st));

        // 1. block starts
        uint64_t nchunks = std::max<uint64_t>(1, (end_d - s0.bit + CHUNK_BITS - 1) / CHUNK_BITS);
        CK(cudaMemsetAsync(g.d_first, 0xff, nchunks * 8, st));
        CK(cudaMemsetAsync(g.d_ncand, 0, sizeof(unsigned), st));
        uint64_t word_lo = s0.bit / 32, word_hi = std::min<uint64_t>((end_d + 31) / 32, (dsize + 3) / 4);
        if (word_hi > word_lo)
            k_prefilter<<<blocks_for(word_hi - word_lo, 256), 256, 0, st>>>((const uint32_t*)g.d_in, word_lo,
                                                                            word_hi - word_lo, s0.bit, end_d, g.d_cand,
                                                                            g.d_ncand, g.maxcand);
        CK(cudaGetLastError());
        unsigned ncand = 0;
        CK(cudaMemcpyAsync(&ncand, g.d_ncand, sizeof(unsigned), cudaMemcpyDeviceToHost, st));
        CK(cudaStreamSynchronize(st));
        // more survivors than room only drops some chunks' starts: fewer, longer jobs
        ncand = std::min(ncand, g.maxcand);
        if (ncand)
            k_verify<<<blocks_for(ncand, 256), 256, 0, st>>>(g.d_in, dsize, g.d_cand, ncand, s0.bit,
                                                            (unsigned long long*)g.d_first);
        CK(cudaGetLastError());
        CK(cudaMemcpyAsync(g.h_first, g.d_first, nchunks * 8, cudaMemcpyDeviceToHost, st));
        CK(cudaStreamSynchronize(st));
        std::vector<gi::Start> starts = block_starts(g.h_first, nchunks, s0);

        // 2. probes
        std::vector<Probe> pts = probe_points(starts, end_d, PROBE_SPACING);
        if (pts.size() > g.njobs_max / 4) pts.resize(g.njobs_max / 4);
        std::vector<gi::Start> probed(pts.size());
        if (!pts.empty()) {
            CK(cudaMemcpyAsync(g.d_probes, pts.data(), pts.size() * sizeof(Probe), cudaMemcpyHostToDevice, st));
            k_probe<<<blocks_for(pts.size(), 8), DECODE_BLOCK, DECODE_SHMEM, st>>>(g.d_in, dsize, g.d_probes,
                                                                                  (int)pts.size(), g.d_probed);
            CK(cudaGetLastError());
            CK(cudaMemcpyAsync(probed.data(), g.d_probed, pts.size() * sizeof(gi::Start), cudaMemcpyDeviceToHost, st));
            CK(cudaStreamSynchronize(st));
        }
        starts = merge_starts(std::move(starts), probed, s0.bit, end_d);
        CK(cudaEventRecord(g.ev[2], st));
        CK(cudaEventSynchronize(g.ev[2]));
        double up_s = elapsed(g.ev[0], g.ev[1]), find_s = elapsed(g.ev[1], g.ev[2]);

        // 3-4. decode and stitch
        Jobs j = make_jobs(starts, end_d, ratio, g.pool, g.njobs_max);
        size_t n0 = j.start.size();
        CK(cudaMemcpyAsync(g.d_targets, starts.data(), n0 * sizeof(gi::Start), cudaMemcpyHostToDevice, st));
        std::vector<int> order(n0);
        std::iota(order.begin(), order.end(), 0);
        std::sort(order.begin(), order.end(), [&](int a, int b) { return j.cap[a] > j.cap[b]; });
        CK(cudaMemcpyAsync(g.d_order, order.data(), n0 * 4, cudaMemcpyHostToDevice, st));
        CK(cudaMemsetAsync(g.d_nstored, 0, sizeof(unsigned), st));
        gi::StoredCopies stored{g.d_stored, g.d_nstored, g.maxstored};
        int rounds = 0;
        Chain c = decode_all(j, g.seg_out_max, g.debug, rounds, [&](size_t a, size_t b) {
            size_t n = b - a;
            CK(cudaMemcpyAsync(g.d_jobs + a, j.start.data() + a, n * sizeof(gi::Start), cudaMemcpyHostToDevice, st));
            CK(cudaMemcpyAsync(g.d_tfirst + a, j.tfirst.data() + a, n * 4, cudaMemcpyHostToDevice, st));
            CK(cudaMemcpyAsync(g.d_off + a, j.off.data() + a, n * 8, cudaMemcpyHostToDevice, st));
            CK(cudaMemcpyAsync(g.d_cap + a, j.cap.data() + a, n * 4, cudaMemcpyHostToDevice, st));
            k_decode<<<blocks_for(n, 8), DECODE_BLOCK, DECODE_SHMEM, st>>>(
                g.d_in, dsize, g.d_jobs, g.d_tfirst, g.d_off, g.d_cap, g.d_targets, (int)n0,
                a == 0 ? g.d_order : nullptr, (int)a, (int)n, g.d_out16, g.d_res, W == 0, stop_d, stored);
            CK(cudaGetLastError());
            CK(cudaMemcpyAsync(j.res.data() + a, g.d_res + a, n * sizeof(gi::Result), cudaMemcpyDeviceToHost, st));
            CK(cudaStreamSynchronize(st));
        });
        CK(cudaEventRecord(g.ev[3], st));
        stats.rounds += rounds;
        stats.jobs += j.start.size();
        if (g.debug)
            fprintf(stderr, "  segment %u at bit %llu: %llu KB in, %zu starts (%zu probes), %zu jobs, %d rounds, %zu pieces, %llu bytes out, walk %d\n",
                    stats.segments, (unsigned long long)cur.bit, (unsigned long long)((hi - lo) >> 10), n0, pts.size(),
                    j.start.size(), rounds, c.s.off.size(), (unsigned long long)c.s.total, (int)c.walk);
        if (c.walk == FAIL) {
            g.err = c.status == gi::ERR_INPUT ? "the input ends inside the stream"
                    : c.status < 0             ? "the pieces did not chain (internal error)"
                                               : "invalid Deflate data";
            g.err += " (near byte " + std::to_string(host(std::max(c.fail_bit, HDR_BITS)) / 8) + ")";
            writer.finish();
            return c.status == gi::ERR_INPUT ? GI_ERR_TRUNCATED : GI_ERR_DATA;
        }

        // 5. resolve
        uint64_t total = c.s.total, n = W + total;
        unsigned nstored = 0;
        CK(cudaMemcpyAsync(&nstored, g.d_nstored, sizeof(unsigned), cudaMemcpyDeviceToHost, st));
        CK(cudaStreamSynchronize(st));
        nstored = std::min(nstored, g.maxstored);
        if (nstored) k_stored<<<nstored, 256, 0, st>>>(g.d_stored, nstored);
        CK(cudaGetLastError());
        if (W) k_window<<<blocks_for(W, 256), 256, 0, st>>>(g.d_e32, g.d_window, W);
        int np = (int)c.s.off.size();
        for (uint64_t& d : c.s.dst) d += W;
        CK(cudaMemsetAsync(g.d_bad, 0, sizeof(int), st));
        if (np) {
            CK(cudaMemcpyAsync(g.d_poff, c.s.off.data(), np * 8, cudaMemcpyHostToDevice, st));
            CK(cudaMemcpyAsync(g.d_pdst, c.s.dst.data(), np * 8, cudaMemcpyHostToDevice, st));
            CK(cudaMemcpyAsync(g.d_plen, c.s.len.data(), np * 4, cudaMemcpyHostToDevice, st));
            k_absolute<<<np, 256, 0, st>>>(g.d_out16, g.d_poff, g.d_plen, g.d_pdst, g.d_e32, g.d_bad);
        }
        CK(cudaGetLastError());
        int bad = 0;
        CK(cudaMemcpyAsync(&bad, g.d_bad, sizeof(int), cudaMemcpyDeviceToHost, st));
        CK(cudaStreamSynchronize(st));
        if (bad) {
            g.err = "invalid Deflate data (a match reaches before the stream's start)";
            writer.finish();
            return GI_ERR_DATA;
        }
        const uint32_t* list = nullptr;  // first round: every entry
        int jumps = 0;
        for (uint64_t pending = n; pending; jumps++) {
            if (jumps == MAX_JUMPS) throw CudaFail{"references did not resolve (internal error)"};
            unsigned* cnt = g.d_nwork + (jumps & 1);
            uint32_t* next = g.d_work[jumps & 1];
            CK(cudaMemsetAsync(cnt, 0, sizeof(unsigned), st));
            k_jump<<<blocks_for(pending, 256), 256, 0, st>>>(g.d_e32, pending, list, next, cnt, g.work_cap);
            CK(cudaGetLastError());
            unsigned left = 0;
            CK(cudaMemcpyAsync(&left, cnt, sizeof(unsigned), cudaMemcpyDeviceToHost, st));
            CK(cudaStreamSynchronize(st));
            if (left > g.work_cap) {
                pending = n;
                list = nullptr;
            } else {
                pending = left;
                list = next;
            }
        }
        if (n) k_narrow<<<blocks_for(n, 256), 256, 0, st>>>(g.d_e32, n, g.d_final);
        CK(cudaGetLastError());
        uint32_t newW = (uint32_t)std::min<uint64_t>(WINDOW, n);
        if (newW) CK(cudaMemcpyAsync(g.d_window, g.d_final + (n - newW), newW, cudaMemcpyDeviceToDevice, st));
        CK(cudaEventRecord(g.ev[4], st));
        CK(cudaEventSynchronize(g.ev[4]));
        double d2h = 0;
        for (uint64_t at = 0; at < total; at += OUT_BUF, buf = (buf + 1) % OUT_BUFS) {
            uint64_t len = std::min<uint64_t>(OUT_BUF, total - at);
            writer.wait_free(buf);
            CK(cudaEventRecord(g.ev[5], st));
            CK(cudaMemcpyAsync(g.h_out[buf], g.d_final + W + at, len, cudaMemcpyDeviceToHost, st));
            CK(cudaEventRecord(g.ev[1], st));
            CK(cudaEventSynchronize(g.ev[1]));
            d2h += elapsed(g.ev[5], g.ev[1]);
            writer.push(buf, len);
        }
        stats.out_size += total;
        stats.segments++;
        stats.upload_s += up_s;
        stats.find_s += find_s;
        stats.decode_s += elapsed(g.ev[2], g.ev[3]);
        stats.resolve_s += elapsed(g.ev[3], g.ev[4]);
        stats.download_s += d2h;

        if (c.walk == DONE) {
            stats.in_used = (host(c.end_bit) + 7) / 8;
            break;
        }
        // CUT: the next segment starts where the chain stopped
        gi::Start nx = c.next;
        nx.bit = host(c.next.bit);
        nx.header = nx.mode == gi::IN_DYNAMIC ? (c.next.header < HDR_BITS ? cur.header : host(c.next.header))
                    : nx.mode == gi::AT_BLOCK ? nx.bit
                                              : UINT64_MAX;
        if (nx.bit <= cur.bit && total == 0) {
            g.err = "no progress (internal error)";
            writer.finish();
            return GI_ERR_DATA;
        }
        if (nx.bit > cur.bit)
            ratio = std::clamp((double)total / ((double)(nx.bit - cur.bit) / 8), 1.0, 1032.0);
        W = newW;
        cur = nx;
    }
    if (writer.finish()) {
        g.err = "stopped by the caller";
        return GI_ERR_ABORTED;
    }
    stats.seconds = since(t_start);
    return GI_OK;
}

}  // namespace

extern "C" {

GI_API uint32_t gi_abi_version(void) { return GI_ABI_VERSION; }

GI_API int gi_create(const gi_config* cfg, gi_ctx** out, char* err, size_t err_len) {
    auto fail = [&](int code, const std::string& m) {
        if (err && err_len) snprintf(err, err_len, "%s", m.c_str());
        return code;
    };
    if (!out) return fail(GI_ERR_ARG, "no context pointer");
    *out = nullptr;
    gi_config c{sizeof(gi_config), 0, 0, 0};
    if (cfg) {
        if (cfg->size < sizeof(gi_config)) return fail(GI_ERR_ARG, "gi_config too small");
        c = *cfg;
    }
    gi_ctx* g = new gi_ctx;
    try {
        setup(*g, c);
    } catch (const CudaFail& f) {
        g->release();
        delete g;
        return fail(GI_ERR_GPU, f.msg);
    } catch (const std::bad_alloc&) {
        g->release();
        delete g;
        return fail(GI_ERR_GPU, "out of host memory");
    }
    *out = g;
    return GI_OK;
}

GI_API int gi_inflate(gi_ctx* g, const uint8_t* in, uint64_t in_size, uint64_t out_hint, gi_sink sink, void* user,
                      gi_stats* stats) {
    if (!g) return GI_ERR_ARG;
    gi_stats s{};
    g->err.clear();
    int r;
    if (!in && in_size) {
        g->err = "no input";
        r = GI_ERR_ARG;
    } else if (in_size == 0) {
        g->err = "empty input";
        r = GI_ERR_TRUNCATED;
    } else {
        try {
            r = inflate(*g, in, in_size, out_hint, sink, user, s);
        } catch (const CudaFail& f) {
            g->err = f.msg;
            r = GI_ERR_GPU;
        } catch (const std::bad_alloc&) {
            g->err = "out of host memory";
            r = GI_ERR_GPU;
        }
        if (r == GI_ERR_GPU) {
            // leave the device ready for the next call: let the work already queued finish, and
            // clear the error, or that call's first check reports this one again and fails too
            cudaStreamSynchronize(g->st);
            cudaGetLastError();
        }
    }
    if (stats) *stats = s;
    return r;
}

GI_API const char* gi_last_error(const gi_ctx* g) { return g ? g->err.c_str() : "no context"; }

GI_API const char* gi_device_name(const gi_ctx* g) { return g ? g->name.c_str() : ""; }

GI_API void gi_destroy(gi_ctx* g) {
    if (!g) return;
    cudaSetDevice(g->device);
    if (g->st) cudaStreamSynchronize(g->st);
    g->release();
    delete g;
}

}  // extern "C"
