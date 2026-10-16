// match.cu - feasibility test of LZ77 match finding on the GPU for a zstd compressor.
//
//   gpumatch <file> [chunk_mb=128] [--dump]
//
// The input is processed in chunks (the compression window). In each chunk, every position
// is a record (key = the bytes that start there, value = the position); a stable radix sort
// by key (CUB) lines up equal prefixes in position order, so a position's nearest earlier
// occurrences are its predecessors in the sorted order. Four orders are built: by the first
// 4 bytes (the nearest short matches: small offsets are cheap to encode), by the first 8, and
// by hashes of the first 16 and 32 (long matches, usually further back). Each candidate is
// extended by comparing bytes up to a cap; a parser extends the capped ones on demand, which
// costs in total no more than the output's length. Every position keeps K candidates:
//   0  the nearest match of 3+ bytes (4-byte order, nearest predecessor)
//   1  the longest among the nearest predecessors in the 4- and 8-byte orders
//   2  the longest among the nearest predecessors in the 16- and 32-byte orders
// Reports the time per stage and the coverage of a greedy parse. --dump writes <file>.cand3:
// K records (uint32 offset, uint32 length; 0 0: none) per position.
#include <cub/cub.cuh>
#include <cuda_runtime.h>

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#define CK(x)                                                                                  \
    do {                                                                                       \
        cudaError_t e_ = (x);                                                                  \
        if (e_ != cudaSuccess) {                                                               \
            fprintf(stderr, "%s:%d %s: %s\n", __FILE__, __LINE__, #x, cudaGetErrorString(e_)); \
            exit(1);                                                                           \
        }                                                                                      \
    } while (0)

constexpr uint32_t CAP = 64;  // candidates are extended this far; parsers extend the rest on demand
constexpr int K = 3;          // candidates kept per position
constexpr int PRED = 4;       // predecessors checked per order

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

// the buffer is padded with 32 zero bytes, so reading past n is safe
__global__ void k_keys(const uint8_t* d, uint32_t n, uint32_t* k4, uint64_t* k8, uint64_t* k16, uint64_t* k32,
                       uint32_t* pos) {
    uint32_t i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= n) return;
    uint64_t w[4];
    memcpy(w, d + i, 32);
    k4[i] = be32(d + i);
    k8[i] = (uint64_t)be32(d + i) << 32 | be32(d + i + 4);
    uint64_t h = mix(0x12345678ull, w[0]);
    h = mix(h, w[1]);
    k16[i] = h;
    h = mix(h, w[2]);
    h = mix(h, w[3]);
    k32[i] = h;
    pos[i] = i;
}

// bytes in common from a and b, up to lim
__device__ __forceinline__ uint32_t extend(const uint8_t* d, uint32_t a, uint32_t b, uint32_t lim) {
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

// For the position at rank r in a sorted order: its nearest predecessors with the same key are
// its nearest earlier occurrences. Writes the nearest match to slot `slot_near` (if >= 0) and
// the longest found to slot `slot_best`, when longer than what is there. (Not `near`/`best`
// alone: windows.h defines `near` away.)
template <class K_>
__global__ void k_candidates(const uint8_t* d, uint32_t n, const K_* keys, const uint32_t* pos, int minlen,
                             int slot_near, int slot_best, Cand* cand) {
    uint32_t r = blockIdx.x * blockDim.x + threadIdx.x;
    if (r >= n) return;
    uint32_t i = pos[r];
    K_ k = keys[r];
    uint32_t best_len = 0, best_off = 0;
    for (int p = 1; p <= PRED && (uint32_t)p <= r; p++) {
        if (keys[r - p] != k) break;
        uint32_t j = pos[r - p];  // j < i: the sort is stable and the input was in position order
        uint32_t len = extend(d, j, i, min(CAP, n - i));
        if (p == 1 && slot_near >= 0 && len >= 3) cand[i * K + slot_near] = Cand{i - j, len};
        if (len > best_len) {
            best_len = len;
            best_off = i - j;
        }
    }
    if (best_len >= (uint32_t)minlen && best_len > cand[i * K + slot_best].len)
        cand[i * K + slot_best] = Cand{best_off, best_len};
}

using clk = std::chrono::steady_clock;
static float ms(cudaEvent_t a, cudaEvent_t b) {
    float t;
    CK(cudaEventElapsedTime(&t, a, b));
    return t;
}

static uint32_t extend_host(const uint8_t* d, uint32_t a, uint32_t b, uint32_t lim) {
    uint32_t len = 0;
    while (len < lim && d[a + len] == d[b + len]) len++;
    return len;
}

int main(int argc, char** argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: gpumatch <file> [chunk_mb=128] [--dump]\n");
        return 2;
    }
    bool dump = false;
    size_t chunk = 128u << 20;
    for (int i = 2; i < argc; i++) {
        if (std::string(argv[i]) == "--dump") dump = true;
        else chunk = (size_t)atoi(argv[i]) << 20;
    }
    FILE* f = fopen(argv[1], "rb");
    if (!f) return 1;
    fseek(f, 0, SEEK_END);
    size_t size = (size_t)_ftelli64(f);
    fseek(f, 0, SEEK_SET);
    std::vector<uint8_t> data(size + 32, 0);
    if (fread(data.data(), 1, size, f) != size) return 1;
    fclose(f);
    chunk = std::min(chunk, size);

    uint8_t* d_data;
    uint32_t *d_k4, *d_k4s, *d_pos, *d_poss;
    uint64_t *d_k8, *d_k16, *d_k32, *d_k8s;
    Cand* d_cand;
    CK(cudaMalloc(&d_data, chunk + 32));
    CK(cudaMalloc(&d_k4, chunk * 4));
    CK(cudaMalloc(&d_k4s, chunk * 4));
    CK(cudaMalloc(&d_k8, chunk * 8));
    CK(cudaMalloc(&d_k16, chunk * 8));
    CK(cudaMalloc(&d_k32, chunk * 8));
    CK(cudaMalloc(&d_k8s, chunk * 8));
    CK(cudaMalloc(&d_pos, chunk * 4));
    CK(cudaMalloc(&d_poss, chunk * 4));
    CK(cudaMalloc(&d_cand, chunk * K * sizeof(Cand)));
    size_t tmp4 = 0, tmp8 = 0;
    cub::DeviceRadixSort::SortPairs(nullptr, tmp4, d_k4, d_k4s, d_pos, d_poss, (int)chunk);
    cub::DeviceRadixSort::SortPairs(nullptr, tmp8, d_k8, d_k8s, d_pos, d_poss, (int)chunk);
    void* d_tmp;
    CK(cudaMalloc(&d_tmp, std::max(tmp4, tmp8)));
    cudaEvent_t ev[10];
    for (auto& e : ev) CK(cudaEventCreate(&e));
    std::vector<Cand> cand(chunk * K);
    FILE* out = nullptr;
    if (dump) out = fopen((std::string(argv[1]) + ".cand3").c_str(), "wb");
    double t[9] = {};
    uint64_t have3 = 0, have8 = 0, sum_len = 0, sum_off = 0, matched_bytes = 0;
    auto t0 = clk::now();
    unsigned blocks = (unsigned)((chunk + 255) / 256);
    for (size_t at = 0; at < size; at += chunk) {
        uint32_t n = (uint32_t)std::min(chunk, size - at);
        blocks = (n + 255) / 256;
        CK(cudaMemcpy(d_data, data.data() + at, n + 32, cudaMemcpyHostToDevice));
        CK(cudaMemset(d_cand, 0, n * K * sizeof(Cand)));
        CK(cudaEventRecord(ev[0]));
        k_keys<<<blocks, 256>>>(d_data, n, d_k4, d_k8, d_k16, d_k32, d_pos);
        CK(cudaEventRecord(ev[1]));
        size_t tb = tmp4;
        cub::DeviceRadixSort::SortPairs(d_tmp, tb, d_k4, d_k4s, d_pos, d_poss, (int)n);
        CK(cudaEventRecord(ev[2]));
        k_candidates<<<blocks, 256>>>(d_data, n, d_k4s, d_poss, 3, 0, 1, d_cand);
        CK(cudaEventRecord(ev[3]));
        const uint64_t* keys[3] = {d_k8, d_k16, d_k32};
        const int minlen[3] = {8, 16, 32}, slot[3] = {1, 2, 2};
        for (int s = 0; s < 3; s++) {
            tb = tmp8;
            cub::DeviceRadixSort::SortPairs(d_tmp, tb, keys[s], d_k8s, d_pos, d_poss, (int)n);
            CK(cudaEventRecord(ev[4 + 2 * s]));
            k_candidates<<<blocks, 256>>>(d_data, n, d_k8s, d_poss, minlen[s], -1, slot[s], d_cand);
            CK(cudaEventRecord(ev[5 + 2 * s]));
        }
        CK(cudaEventSynchronize(ev[9]));
        CK(cudaGetLastError());
        for (int s = 0; s < 9; s++) t[s] += ms(ev[s], ev[s + 1]);
        CK(cudaMemcpy(cand.data(), d_cand, n * K * sizeof(Cand), cudaMemcpyDeviceToHost));
        if (out) fwrite(cand.data(), sizeof(Cand), (size_t)n * K, out);
        // a greedy parse's coverage (how many bytes land in matches), extending capped ones
        const uint8_t* d = data.data() + at;
        for (uint32_t i = 0; i < n;) {
            Cand c = {0, 0};
            for (int s = 0; s < K; s++)
                if (cand[i * K + s].len > c.len) c = cand[i * K + s];
            if (c.len >= 3) {
                if (c.len == CAP) c.len = extend_host(d, i - c.off, i, n - i);
                have3++;
                if (c.len >= 8) have8++;
                sum_len += c.len;
                sum_off += c.off;
                matched_bytes += c.len;
                i += c.len;
            } else {
                i++;
            }
        }
    }
    if (out) fclose(out);
    double wall = std::chrono::duration<double>(clk::now() - t0).count();
    double gpu_ms = 0;
    for (double x : t) gpu_ms += x;
    printf("%s: %zu MB in chunks of %zu MB\n", argv[1], size >> 20, chunk >> 20);
    printf("GPU time %.0f ms = %.2f GB/s  [keys %.0f · sort4 %.0f + cand %.0f · sort8 %.0f + cand %.0f · sort16 %.0f + cand %.0f · sort32 %.0f + cand %.0f ms]; wall %.2f s\n",
           gpu_ms, size / gpu_ms / 1e6, t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7], t[8], wall);
    printf("greedy parse: %llu matches cover %.1f%% of the bytes; mean length %.1f, mean offset %.0f; %.1f%% of matches are 8+\n",
           (unsigned long long)have3, 100.0 * matched_bytes / size, (double)sum_len / std::max<uint64_t>(have3, 1),
           (double)sum_off / std::max<uint64_t>(have3, 1), 100.0 * have8 / std::max<uint64_t>(have3, 1));
    return 0;
}
