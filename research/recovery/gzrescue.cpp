// gzrescue - command line over the library in gpuinflate/rescue.cpp.
//
//   gzrescue <damaged.gz> <out> [--report out.json] [--mask out.mask] [--fill 0]
//
// Writes the recovered data, and with --mask one byte per output byte (1 exact, 2 in a damaged
// block, 0 unknown) and with --report the pieces, members and notes as JSON.
#define GR_STATIC
#include "../../gpuinflate/rescue.h"

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

static std::string json_escape(const std::string& s) {
    std::string o;
    for (char c : s) {
        if (c == '"' || c == '\\') o += '\\';
        o += c;
    }
    return o;
}

struct Out {
    FILE* data;
    FILE* mask;
};

static int sink(void* user, const uint8_t* data, const uint8_t* mask, size_t len) {
    Out* o = (Out*)user;
    if (fwrite(data, 1, len, o->data) != len) return 1;
    if (o->mask && fwrite(mask, 1, len, o->mask) != len) return 1;
    return 0;
}

int main(int argc, char** argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: gzrescue <damaged.gz> <out> [--report out.json] [--mask out.mask] [--fill 0]\n");
        return 2;
    }
    std::string report_path, mask_path;
    gr_options opt{sizeof(gr_options), 0};
    for (int i = 3; i < argc; i++) {
        std::string a = argv[i];
        if (a == "--report" && i + 1 < argc) report_path = argv[++i];
        else if (a == "--mask" && i + 1 < argc) mask_path = argv[++i];
        else if (a == "--fill" && i + 1 < argc) opt.fill = (uint8_t)atoi(argv[++i]);
    }
    FILE* f = fopen(argv[1], "rb");
    if (!f) {
        fprintf(stderr, "cannot open %s\n", argv[1]);
        return 1;
    }
    fseek(f, 0, SEEK_END);
    uint64_t size = (uint64_t)_ftelli64(f);
    fseek(f, 0, SEEK_SET);
    std::vector<uint8_t> in(size);
    if (size && fread(in.data(), 1, size, f) != size) return 1;
    fclose(f);

    Out o{fopen(argv[2], "wb"), mask_path.empty() ? nullptr : fopen(mask_path.c_str(), "wb")};
    if (!o.data) {
        fprintf(stderr, "cannot write %s\n", argv[2]);
        return 1;
    }
    gr_result* res = nullptr;
    int rc = gr_rescue(in.data(), size, &opt, sink, &o, &res);
    fclose(o.data);
    if (o.mask) fclose(o.mask);
    if (rc == GR_ERR_NO_DEFLATE) {
        fprintf(stderr, "no Deflate block could be recognised\n");
        return 1;
    }
    if (rc != GR_OK) {
        fprintf(stderr, "gr_rescue failed: %d\n", rc);
        return 1;
    }
    const gr_stats* s = gr_result_stats(res);
    const gr_piece* pieces;
    uint32_t np = gr_result_pieces(res, &pieces);
    const char* const* notes;
    uint32_t nn = gr_result_notes(res, &notes);

    printf("%s: %llu bytes in, %llu bytes out; %u block header%s found\n", argv[1], (unsigned long long)s->input_size,
           (unsigned long long)s->output_size, s->headers, s->headers == 1 ? "" : "s");
    printf("  %u piece%s, %u damaged, %u member%s\n", s->pieces, s->pieces == 1 ? "" : "s", s->damaged, s->members,
           s->members == 1 ? "" : "s");
    printf("  %llu bytes exact, %llu in damaged blocks (partly right, from their start), %llu unknown\n",
           (unsigned long long)s->exact_bytes, (unsigned long long)s->suspect_bytes, (unsigned long long)s->unknown_bytes);
    for (uint32_t k = 0; k < np; k++)
        if (pieces[k].suspect)
            printf("  damaged block at input byte %llu, output offset %llu: %s\n", (unsigned long long)pieces[k].in_byte_start,
                   (unsigned long long)pieces[k].out_offset, pieces[k].why);
    if (s->crc_check >= 0) printf("  member CRC %s\n", s->crc_check ? "ok: the data is intact" : "MISMATCH: an undetectable error somewhere");
    for (uint32_t k = 0; k < nn; k++) printf("  note: %s\n", notes[k]);

    if (!report_path.empty()) {
        FILE* rp = fopen(report_path.c_str(), "w");
        if (rp) {
            fprintf(rp, "{\n \"input\": \"%s\",\n \"input_size\": %llu,\n \"output_size\": %llu,\n \"headers_found\": %u,\n",
                    json_escape(argv[1]).c_str(), (unsigned long long)s->input_size, (unsigned long long)s->output_size, s->headers);
            fprintf(rp, " \"pieces\": [\n");
            for (uint32_t k = 0; k < np; k++) {
                const gr_piece& p = pieces[k];
                fprintf(rp, "  {\"out_offset\": %llu, \"length\": %llu, \"decoded\": %llu, \"known\": %llu, \"suspect\": %s, "
                        "\"why\": \"%s\", \"blocks\": %u, \"symbols\": %u, \"in_byte_start\": %llu, \"in_byte_end\": %llu, \"status\": %d}%s\n",
                        (unsigned long long)p.out_offset, (unsigned long long)p.length, (unsigned long long)p.decoded,
                        (unsigned long long)p.known, p.suspect ? "true" : "false", json_escape(p.why).c_str(), p.blocks, p.symbols,
                        (unsigned long long)p.in_byte_start, (unsigned long long)p.in_byte_end, p.status, k + 1 < np ? "," : "");
            }
            fprintf(rp, " ],\n \"members\": [{\"crc_check\": %d, \"length_off\": %lld}],\n \"notes\": [", s->crc_check, (long long)s->length_off);
            for (uint32_t k = 0; k < nn; k++) fprintf(rp, "%s\"%s\"", k ? ", " : "", json_escape(notes[k]).c_str());
            fprintf(rp, "]\n}\n");
            fclose(rp);
        }
    }
    gr_free(res);
    return 0;
}
