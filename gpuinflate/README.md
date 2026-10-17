# gpuinflate: one Deflate stream, decompressed in parallel on the GPU

A library (`gpuinflate.dll`, `libgpuinflate.so`; C interface in `gpuinflate.h`) that decompresses a single raw
Deflate stream (a gzip member, a large ZIP entry) on an NVIDIA GPU, with no index and no change
to the format. gpuzip uses it for `.gz`, `.tar.gz` and large ZIP entries. It grew out of the
prototype in `../research/gpu-inflate`, whose README explains the algorithm and its results.

What the library adds to the prototype:

- **Streams of any size.** The stream is decoded in segments of compressed input (up to 64 MB,
  sized from the stream's ratio so each yields at most about 256 MB). A segment starts from an
  exact decoder state (bit position, block type and tables, BFINAL) and ends wherever its chain
  of pieces stops: the stream's last block, the segment's stop bit, or its output budget. The
  32 KB before a segment stay on the GPU for its back-references, and positions are 64-bit, so
  the output size has no limit (tested to 6.4 GB). A segment that starts inside a dynamic block
  whose header lies in an earlier segment gets a copy of that header in front of its input.
- **Output in order, while decoding goes on.** The output reaches the caller's callback in
  16 MB pieces from a thread of the library's own, through 4 pinned buffers; the input goes up
  through 2 more. About 80 MB of pinned host memory, ready in a few milliseconds.
- **Bounded kernels.** A decoder writes at most 4 M symbols and decodes at most 65,536 blocks
  per kernel, then continues in the next round, so no kernel comes near Windows' 2-second limit
  even on hostile input.
- **Hardened against bad input.** Every read past the input returns zeros and is reported
  (truncated stream); every write is checked against the decoder's buffer; matches reaching
  before the stream's start are caught when references are resolved; when there is more of
  something than room for it (block-start candidates, stored blocks, jobs, rounds), the library
  degrades to fewer, longer jobs or a shorter segment instead of failing.
- **Graceful errors.** Invalid data, a truncated stream, a callback that asks to stop, and GPU
  problems (no device, out of memory, a CUDA error) each have their own return code; the caller
  decodes on the CPU after a GPU problem.

## Results

Same machine as the prototype (RTX 5080, Ryzen 7 9700X). `gi_test run`, output copied back to
pinned host memory but given to no callback, best of 3:

| Stream (128 MB of output unless noted) | Prototype | Library |
|---|---|---|
| 7-Zip `-mx5` | 5.09 GB/s | 4.95 GB/s |
| zlib-ng level 6 | 4.96 | 4.72 |
| zlib-ng level 9 | 4.98 | 4.32 |
| zlib-ng level 1 (one 72 MB fixed-Huffman block) | 3.41 | 3.36 |
| libdeflate level 12 | 4.41 | 4.09 |
| text with 1 MB random stretches, 100 MB | 2.95 | 2.30 |
| a 64-byte pattern repeated, 64 MB | 2.34 | 2.53 |
| 1 MB of text | 0.25 | 0.25 |

The streams of 128 MB now take 2 segments (with no size hint the first segment is sized for a
4:1 ratio); gpuzip passes the gzip trailer's size as the hint and gets 1. Through gpuzip, with the
CRC checked and the CUDA context made (about 80 ms per process), a 6.4 GB `.gz` tests in 1.9 s
(3.3 GB/s), 5.3x faster than one CPU thread with zlib-rs.

## Checks

- `run_all.ps1 [-Extra "--seg-in 32 --seg-out 8"]`: the eight streams, byte for byte; with
  tiny segments, thousands of segment boundaries (1,600 for the 7-Zip stream).
- `fuzz_all.ps1`: 4,800 damaged streams (bit flips, overwritten bytes and spans, truncation,
  copied spans) of eight kinds (levels 1/6/9, fixed only, Huffman only, stored, mixed, tiny),
  with tiny and normal segments. The GPU must agree with the same decoder run sequentially on
  the CPU: the same error class, or the same bytes and the same end of stream. 0 disagreements.
- The same fuzzing under `compute-sanitizer --tool memcheck`: 0 errors.
- `make_big_gz.py`: a gzip file with 6 GiB of output (the size field wraps; 41 segments).

## Building

Needs the CUDA toolkit (13.x) and the Visual Studio Build Tools on Windows (`build.cmd`), or
g++ on Linux (`build.sh`, the same arguments). macOS has no CUDA: there `build.sh` builds only
gzrescue.

```
build.cmd             this machine's GPU only
build.cmd release     Turing (RTX 20) and newer, plus PTX for GPUs newer than this toolkit
build.cmd rescue      gzrescue only (CPU; no CUDA toolkit needed)
```

The CUDA runtime is linked statically, so the library needs only the NVIDIA driver. Put
`gpuinflate.dll` (`libgpuinflate.so`) next to gpuzip (or point `GPUZIP_INFLATE_DIR` at its
folder).

```
python make_fuzz_inputs.py %TEMP%\gpuzip-bench\inflate\text.bin %TEMP%\gpuzip-bench\fuzz
gi_test run  <stream.deflate> [expected] [--runs N] [--seg-in KB] [--seg-out MB]
gi_test fuzz <stream.deflate> <iterations> [--seed S] [--seg-in KB] [--seg-out MB]
```

`GI_DEBUG=1` prints the setup times and, per segment, its starts, jobs, rounds and pieces.

## Interface

```c
gi_config cfg = {sizeof cfg, 0 /* device */, 0, 0};
gi_ctx* ctx;
gi_create(&cfg, &ctx, err, sizeof err);         /* GPU buffers: about 3 GB with 256 MB segments */
gi_inflate(ctx, in, in_size, size_hint, sink, user, &stats);   /* stats.in_used: where it ended */
gi_destroy(ctx);
```

The sink receives the output in order and returns nonzero to stop. Bytes after the stream's
last block are allowed (gzip trailers, further members); `stats.in_used` says where the stream
ended. A context serves one call at a time.

Files: `inflate.cuh` (the decoder, CPU and GPU), `gpuinflate.cu` (segments, kernels, the
interface), `gi_test.cpp` (checks and fuzzing).
