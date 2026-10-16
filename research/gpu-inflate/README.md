# gpu-inflate: one Deflate stream, decompressed in parallel on the GPU

A prototype that decompresses a **single** standard Deflate stream (a large ZIP entry, a gzip
member) on an NVIDIA GPU, with no index and no change to the format. Existing tools either run
one thread per stream (zlib, libdeflate, 7-Zip, nvCOMP's batched API) or split the stream on the
CPU (rapidgzip). This one splits it on the GPU, including where rapidgzip cannot.

This folder is the experiment. The library gpuzip uses, which decodes streams of any size in
segments and is hardened and fuzzed against bad input, is in [`../../gpuinflate`](../../gpuinflate).

## Results

RTX 5080 (84 SMs), Ryzen 7 9700X (8 cores / 16 threads), Windows 11. Each stream decodes to
128 MB of text unless noted; every output is checked byte for byte. GPU times include copying
the input to the GPU and the output back (pinned memory, buffers allocated once beforehand).

| Stream | zlib-ng, 1 thread | rapidgzip 0.16, 16 threads | GPU |
|---|---|---|---|
| 7-Zip `-mx5` (3,555 blocks) | 0.66 GB/s | 1.05 GB/s | **5.09 GB/s** |
| zlib-ng level 6 | 0.67 | 0.95 | **4.96** |
| zlib-ng level 9 | 0.65 | 0.92 | **4.98** |
| zlib-ng level 1 (one 72 MB fixed-Huffman block) | 0.69 | 0.19 | **3.41** |
| libdeflate level 12 (449 large blocks) | 0.78 | 1.01 | **4.41** |
| text with 1 MB random stretches (stored blocks), 100 MB | 1.00 | 1.38 | **2.95** |
| a 64-byte pattern repeated (343:1), 64 MB | **3.49** | 1.42 | 2.34 |
| 1 MB of text | **0.54** | 0.27 | 0.25 |

Below about 10 MB, or on extremely repetitive data, one CPU thread is still faster.

## How it works

1. **Find block starts.** Every bit position gets a cheap test that runs the same way in every
   thread: a dynamic block header whose code-length code is complete, as zlib requires. About
   0.09% of positions survive; those get zlib's full header checks. Each 4 KB chunk keeps its
   first hit. False hits are harmless (step 3).
2. **Probe long blocks.** Fixed-Huffman blocks have no header to find, and some encoders write
   huge blocks. Inside any stretch longer than 32 KB, probes decode from 4 KB before a point with
   the tables of the block the stretch begins in, restarting one bit later on nonsense. Huffman
   codes resynchronize, so the first symbol boundary after the point is almost always real.
3. **Decode every start at once.** Each decoder runs until it lands exactly on a later start
   (with the same tables), so a false start is simply passed over. One decoder per warp: 32
   decoders in one warp wait for each other on every branch (measured 5x slower). Longest jobs
   go first, and registers are capped so more decoders fit on an SM at once.
4. **Write references, not copies.** A match writes "the byte d positions back" into each output
   position instead of copying, so decoding never waits on memory reads. Stored blocks are noted
   and copied by a separate kernel. A decoder whose buffer fills stops at a symbol boundary and
   reports its state (position, block, tables' header); a new decoder continues from there.
5. **Stitch and resolve.** The pieces that connect from bit 0 are laid end to end; every
   reference becomes an absolute position, and pointer jumping (up to 8 hops in place, then only
   the unresolved entries) resolves them all in 2 to 4 rounds.

## Measured dead ends

- Lookup tables in constant memory: serialized when 32 lanes read different entries.
- Resolving references by following chains: 72% of this text's output is copies of copies,
  717 pieces deep on average; pointer jumping is needed.
- 9-bit first-level tables: more slow-path decodes than the occupancy gained.
- No register cap (87 per thread, 2 blocks per SM) or a tight one (6 blocks per SM, spills).

## Related work

rapidgzip and pugz (parallel speculative gzip decoding on CPU cores), Gompresso (LZ77 on the GPU
with multi-round resolution, in its own format), nvCOMP (many independent streams at once),
GPU Huffman decoding by self-synchronization (Weißenberger and Schmidt). We did not find a
published GPU decoder for single standard Deflate streams.

## Running it

Needs the CUDA toolkit (13.x), Visual Studio Build Tools and an NVIDIA GPU (built for sm_120).

```
python make_inputs.py %TEMP%\gpuzip-bench %TEMP%\gpuzip-bench\inflate     (from bench/'s large.zip)
python make_hard_inputs.py %TEMP%\gpuzip-bench\inflate
build.cmd
powershell -File run_all.ps1
python vs_rapidgzip.py %TEMP%\gpuzip-bench\inflate                       (pip install rapidgzip)
```

`gpuinflate <stream.deflate> <expected.bin> [chunk_kb]` runs one stream three ways: our decoder
on one CPU thread, the speculative algorithm on CPU threads, and on the GPU (three runs).
`GI_DEBUG=1` prints the piece chain and probe timing. `sweep_regs.ps1` compares register caps.

Files: `inflate.cuh` (the decoder, CPU and GPU), `gpuinflate.cu` (the pipeline and kernels).
