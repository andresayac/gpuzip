// gi_test - checks gpuinflate.dll against the same decoder run sequentially on the CPU, and
// fuzzes it with damaged streams.
//
//   gi_test run  <stream.deflate> [expected.bin] [--runs N] [--seg-in KB] [--seg-out MB]
//   gi_test fuzz <stream.deflate> <iterations> [--seed S] [--seg-in KB] [--seg-out MB]
//
// run: decodes the stream once checking every byte (against expected.bin, or else the CPU
// decoder), then N more times without a sink, and prints the speed.
// fuzz: damages the stream at random (bit flips, overwritten bytes and spans, truncation,
// copied spans) and requires the GPU to agree with the CPU: same error class, or the same
// bytes and the same end of stream. Disagreements are saved as fuzz-fail-<n>.deflate.
#include "gpuinflate.h"
#include "inflate.cuh"

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

using clk = std::chrono::steady_clock;
static double since(clk::time_point t) { return std::chrono::duration<double>(clk::now() - t).count(); }

static std::vector<uint8_t> read_file(const char* path) {
    FILE* f = fopen(path, "rb");
    if (!f) {
        fprintf(stderr, "cannot open %s\n", path);
        exit(1);
    }
    fseek(f, 0, SEEK_END);
    size_t n = (size_t)_ftelli64(f);
    fseek(f, 0, SEEK_SET);
    std::vector<uint8_t> v(n);
    if (n && fread(v.data(), 1, n, f) != n) exit(1);
    fclose(f);
    return v;
}

struct Ref {
    int status;  // a GI_* code
    bool too_big;
    std::vector<uint8_t> out;
    uint64_t in_used;
};

// The stream decoded from bit 0 on one CPU thread, continuing after every full buffer
static Ref reference(const std::vector<uint8_t>& in, uint64_t max_out) {
    std::vector<uint8_t> padded((in.size() + 3) / 4 * 4 + 64, 0);
    memcpy(padded.data(), in.data(), in.size());
    std::vector<uint16_t> out16(max_out + 2 * gi::MAX_SYMBOL_OUT);
    static gi::Decoder* dec = nullptr;
    static uint16_t luts[gi::LUT_U16];
    if (!dec) dec = new gi::Decoder;
    dec->attach(luts);
    Ref ref{GI_OK, false, {}, 0};
    gi::Start s{0, 0, gi::AT_BLOCK, -1};
    uint64_t op = 0;
    for (;;) {
        gi::Result r = dec->run(padded.data(), in.size(), s, nullptr, 0, out16.data() + op,
                                (uint32_t)(out16.size() - op), op == 0);
        op += r.out_len;
        if (r.status == gi::ERR_OVERFLOW) {
            if (out16.size() - op < 64 * 1024) {
                ref.too_big = true;
                return ref;
            }
            s = r.resume;
            continue;
        }
        if (r.status != gi::OK) {
            ref.status = r.status == gi::ERR_INPUT ? GI_ERR_TRUNCATED : GI_ERR_DATA;
            return ref;
        }
        ref.in_used = (r.end_bit + 7) / 8;
        break;
    }
    ref.out.resize(op);
    for (uint64_t i = 0; i < op; i++) {
        uint16_t v = out16[i];
        if (v <= gi::REF) {
            ref.out[i] = (uint8_t)v;
        } else if (v - gi::REF > i) {
            ref.status = GI_ERR_DATA;  // a match before the stream's start
            ref.out.clear();
            return ref;
        } else {
            ref.out[i] = ref.out[i - (v - gi::REF)];
        }
    }
    return ref;
}

static int collect(void* user, const uint8_t* d, size_t n) {
    auto* v = (std::vector<uint8_t>*)user;
    v->insert(v->end(), d, d + n);
    return 0;
}

struct Splitmix {
    uint64_t s;
    uint64_t next() {
        uint64_t z = (s += 0x9e3779b97f4a7c15ull);
        z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
        z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
        return z ^ (z >> 31);
    }
    uint64_t below(uint64_t n) { return n ? next() % n : 0; }
};

static const char* code_name(int c) {
    switch (c) {
        case GI_OK: return "ok";
        case GI_ERR_DATA: return "data";
        case GI_ERR_TRUNCATED: return "truncated";
        case GI_ERR_ABORTED: return "aborted";
        case GI_ERR_GPU: return "gpu";
        default: return "arg";
    }
}

int main(int argc, char** argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: gi_test run <stream> [expected] [--runs N] [--seg-in KB] [--seg-out MB]\n"
                        "       gi_test fuzz <stream> <iterations> [--seed S] [--seg-in KB] [--seg-out MB]\n");
        return 2;
    }
    std::string mode = argv[1];
    std::vector<std::string> pos;
    int runs = 3;
    uint64_t seed = 1;
    gi_config cfg{sizeof(gi_config), 0, 0, 0};
    for (int i = 2; i < argc; i++) {
        std::string a = argv[i];
        if (a == "--runs" && i + 1 < argc) runs = atoi(argv[++i]);
        else if (a == "--seed" && i + 1 < argc) seed = strtoull(argv[++i], nullptr, 10);
        else if (a == "--seg-in" && i + 1 < argc) cfg.segment_in_kb = (uint32_t)atoi(argv[++i]);
        else if (a == "--seg-out" && i + 1 < argc) cfg.segment_out_mb = (uint32_t)atoi(argv[++i]);
        else pos.push_back(a);
    }
    if (pos.empty()) return 2;
    std::vector<uint8_t> in = read_file(pos[0].c_str());
    char err[256] = {};
    gi_ctx* ctx = nullptr;
    auto t_create = clk::now();
    if (int r = gi_create(&cfg, &ctx, err, sizeof err)) {
        fprintf(stderr, "gi_create: %s (%d)\n", err, r);
        return 1;
    }
    printf("%s, context ready in %.0f ms\n", gi_device_name(ctx), since(t_create) * 1e3);

    if (mode == "run") {
        std::vector<uint8_t> expect;
        if (pos.size() > 1) {
            expect = read_file(pos[1].c_str());
        } else {
            auto t = clk::now();
            Ref ref = reference(in, (uint64_t)in.size() * 1100);
            if (ref.status != GI_OK || ref.too_big) {
                fprintf(stderr, "the CPU decoder fails on this stream: %s\n", code_name(ref.status));
                return 1;
            }
            printf("CPU sequential:  %6.3f s\n", since(t));
            expect = std::move(ref.out);
        }
        std::vector<uint8_t> out;
        out.reserve(expect.size());
        gi_stats s{};
        auto t = clk::now();
        int r = gi_inflate(ctx, in.data(), in.size(), 0, collect, &out, &s);
        double sec = since(t);
        bool good = r == GI_OK && out == expect;
        printf("GPU, checked:    %6.3f s  %s  (%llu bytes, in_used %llu of %zu, %u segments, %u rounds, %llu jobs)\n", sec,
               good ? "OK" : "WRONG", (unsigned long long)out.size(), (unsigned long long)s.in_used, in.size(),
               s.segments, s.rounds, (unsigned long long)s.jobs);
        if (r != GI_OK) printf("  error %s: %s\n", code_name(r), gi_last_error(ctx));
        if (!good) {
            size_t i = 0;
            while (i < out.size() && i < expect.size() && out[i] == expect[i]) i++;
            printf("  first difference at byte %zu (got %zu bytes, expected %zu)\n", i, out.size(), expect.size());
            return 1;
        }
        for (int k = 0; k < runs; k++) {
            t = clk::now();
            r = gi_inflate(ctx, in.data(), in.size(), 0, nullptr, nullptr, &s);
            sec = since(t);
            printf("GPU, no sink:    %6.3f s = %5.2f GB/s  [h2d %.1f · find+probe %.1f · decode %.1f · resolve %.1f · d2h %.1f ms]"
                   "  %u segments, %u rounds\n",
                   sec, s.out_size / sec / 1e9, s.upload_s * 1e3, s.find_s * 1e3, s.decode_s * 1e3, s.resolve_s * 1e3,
                   s.download_s * 1e3, s.segments, s.rounds);
            if (r != GI_OK) return 1;
        }
    } else if (mode == "fuzz") {
        int iters = pos.size() > 1 ? atoi(pos[1].c_str()) : 100;
        Splitmix rng{seed};
        int fails = 0, ok = 0, data = 0, trunc = 0, big = 0;
        auto t = clk::now();
        for (int it = 0; it < iters; it++) {
            std::vector<uint8_t> m = in;
            int nmut = 1 + (int)rng.below(4);
            for (int k = 0; k < nmut && !m.empty(); k++) {
                switch (rng.below(6)) {
                    case 0:
                    case 1: m[rng.below(m.size())] ^= (uint8_t)(1u << rng.below(8)); break;
                    case 2: m[rng.below(m.size())] = (uint8_t)rng.next(); break;
                    case 3: {
                        uint64_t a = rng.below(m.size()), n = std::min<uint64_t>(1 + rng.below(64), m.size() - a);
                        for (uint64_t i = 0; i < n; i++) m[a + i] = (uint8_t)rng.next();
                        break;
                    }
                    case 4: m.resize(rng.below(m.size()) + 1); break;
                    case 5: {
                        uint64_t a = rng.below(m.size()), b = rng.below(m.size());
                        uint64_t n = std::min<uint64_t>({1 + rng.below(4096), m.size() - a, m.size() - b});
                        memmove(m.data() + b, m.data() + a, n);
                        break;
                    }
                }
            }
            Ref ref = reference(m, std::max<uint64_t>(64ull << 20, (uint64_t)in.size() * 16));
            std::vector<uint8_t> out;
            gi_stats s{};
            int r = gi_inflate(ctx, m.data(), m.size(), 0, collect, &out, &s);
            if (r == GI_ERR_GPU) {
                printf("iteration %d: GPU error: %s\n", it, gi_last_error(ctx));
                return 1;
            }
            bool agree;
            if (ref.too_big) {
                big++;
                agree = true;
            } else if (ref.status != GI_OK) {
                agree = r == ref.status;
                (ref.status == GI_ERR_DATA ? data : trunc)++;
            } else {
                agree = r == GI_OK && out == ref.out && s.in_used == ref.in_used;
                ok++;
            }
            if (!agree) {
                std::string name = "fuzz-fail-" + std::to_string(fails) + ".deflate";
                FILE* f = fopen(name.c_str(), "wb");
                fwrite(m.data(), 1, m.size(), f);
                fclose(f);
                printf("iteration %d DISAGREES: CPU %s (%zu bytes, in_used %llu), GPU %s (%zu bytes, in_used %llu): %s -> %s\n",
                       it, code_name(ref.status), ref.out.size(), (unsigned long long)ref.in_used, code_name(r),
                       out.size(), (unsigned long long)s.in_used, gi_last_error(ctx), name.c_str());
                if (++fails >= 10) break;
            }
        }
        printf("%d iterations in %.1f s: %d decoded fine, %d invalid data, %d truncated, %d too big to check; %d disagreements\n",
               iters, since(t), ok, data, trunc, big, fails);
        if (fails) return 1;
    } else {
        return 2;
    }
    gi_destroy(ctx);
    return 0;
}
