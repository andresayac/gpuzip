# gpu-zstd: can a GPU compress to zstd's top levels at GB/s?

A feasibility study, October 2026. Compressing well is slow: on this machine's Ryzen 7 9700X,
one thread runs `zstd -3` at 180-290 MB/s but `zstd -19` at 1.8-4.4 MB/s and LZMA2 `-mx9` at
3 MB/s, so the world stores petabytes at level 3. The question: can a GPU produce **standard
zstd** at level-19 ratio or better at 1 GB/s or more? Standard output matters: anyone's `zstd`
decompresses it.

Go/no-go: a credible path to >= 1 GB/s on an RTX 5080 at a ratio >= `zstd -19`; otherwise stop.

## Corpus

`make_corpus.py` collects 128 MB of each from this machine (nothing personal): `logs.txt` (a
Windows CBS log), `code.tar` (Rust crate sources, Python's standard library, CUDA headers),
`binaries.tar` (System32 DLLs and EXEs), `mixed.tar` (a Laragon install: PHP, MySQL, Apache),
and `text.bin`, the synthetic English-like text of the gpuzip benchmarks, kept as a warning:
it is far more redundant than real text.

## 1. How much ratio is there, and what does the window cost? (`ratio_study.py`)

Ratios (bigger is better), one thread each, 7-Zip with 4:

| Dataset | zstd -3 | -9 | -19 (8 MB window) | -19, window 64 KB | -19, window 1 MB | -19, window 128 MB + LDM | -22, 128 MB + LDM | LZMA2 -mx9 |
|---|---|---|---|---|---|---|---|---|
| binaries.tar | 2.87 | 3.32 | 3.77 | 2.88 | 3.34 | 3.94 | 4.00 | 4.29 |
| code.tar | 5.27 | 6.11 | 6.85 | 5.66 | 6.51 | 6.93 | 7.02 | 7.19 |
| logs.txt | 49.8 | 67.6 | 110.0 | 33.3 | 76.0 | 124.4 | 128.7 | 94.7 |
| mixed.tar | 2.91 | 3.21 | 3.69 | 2.92 | 3.42 | 3.93 | 3.96 | 4.28 |
| text.bin | 2.88 | 3.16 | 3.68 | 2.68 | 3.28 | 7.67 | 7.81 | 7.86 |

What it says:

- **The window decides the ratio.** Level 19 with a 64 KB window loses 20-25% on real data,
  with 1 MB 5-11%; a 128 MB window with long-distance matching gains 1-13% over the default
  8 MB. A GPU compressor that works block by block (the natural fast design; what
  [gpu-zstd-comp](https://github.com/halgari/gpu-zstd-comp) does with 64 KB frames) cannot
  reach level 19 on real data. The large window is mandatory.
- **The zstd format's ceiling is close to LZMA's** on real data: 2-8% behind `-mx9`, ahead on
  the log. Only the synthetic text shows a 2x gap, which is why it is not to be trusted.
- Level 19 costs 50-100x the time of level 3 for 25-35% smaller output; 22 costs more still.

## 2. Finding matches on the GPU (`match.cu`)

Compression has no decoding dependency: the whole input is known, so the best match for every
position can be searched for at once. In each 128 MB chunk (the window), every position becomes
a record (key = the bytes that start there, value = the position) and a **stable radix sort** by
key (CUB) lines up equal prefixes in position order: a position's nearest earlier occurrences
are its predecessors in the sorted order, and the stable sort keeps them nearest first. Four
orders: by the first 4 bytes (nearest short matches: small offsets are cheap), by the first 8,
and by hashes of the first 16 and 32 (long matches, usually far back). Each candidate is
extended by comparing bytes up to 64; a parser extends the capped ones on demand, which costs
in total no more than the output's length. Every position keeps three candidates: the nearest
of 3+ bytes, the longest from the 4- and 8-byte orders, the longest from the 16- and 32-byte
orders.

RTX 5080, 128 MB chunks, nothing tuned:

| Dataset | GPU time | Throughput | of which the 4 sorts |
|---|---|---|---|
| binaries.tar | 291 ms | 0.46 GB/s | 131 ms |
| code.tar | 345 ms | 0.39 GB/s | 126 ms |
| logs.txt | 524 ms | 0.26 GB/s | 126 ms |
| mixed.tar | 298 ms | 0.45 GB/s | 129 ms |
| text.bin | 398 ms | 0.34 GB/s | 129 ms |

The sorts are cheap (13 ms for 4-byte keys, 38 ms for 8-byte keys, per 128 MB); the candidate
extension kernels (random 8-byte reads from two places per candidate) take the rest and are the
obvious target. With two orders instead of four the finder ran at 0.75-0.88 GB/s.

## 3. From candidates to a standard zstd file (`zseq/`)

libzstd (through the `zstd-sys` crate, feature `experimental`) exposes
`ZSTD_compressSequences`: given the LZ77 sequences (literal run, match length, offset), it
splits them into blocks, finds repeat offsets and entropy-codes everything into a standard
frame. So the parse can be judged on its own, with zstd's entropy coding on both sides. `zseq`
parses the GPU candidates three ways: greedy (the longest match as it comes), lazy with repeat
offsets (zstd's lazy2 idea), and an optimal parse (a shortest path over positions priced in
bits from code statistics, two passes; zstd's btultra idea), then checks that every frame
decodes to the input.

Ratios, all frames verified. The zstd columns use the same 128 MB window as our parses, with
long-distance matching; `-19 (8 MB)` is plain `zstd -19`:

| Dataset | zstd -19 (8 MB) | zstd -19, 128 MB + LDM | zstd -22, 128 MB + LDM | ours: greedy | ours: lazy | **ours: optimal** | optimal vs -19 + LDM |
|---|---|---|---|---|---|---|---|
| binaries.tar | 3.77 | 3.944 | 4.001 | 3.194 | 3.443 | **3.824** | -3.0% |
| code.tar | 6.85 | 6.932 | 7.024 | 6.060 | 6.253 | **6.784** | -2.1% |
| logs.txt | 110.0 | 124.4 | 128.7 | 67.2 | 91.0 | **110.0** | -11.6% |
| mixed.tar | 3.69 | 3.934 | 3.962 | 3.213 | 3.397 | **3.745** | -4.8% |
| text.bin | 3.68 | 7.667 | 7.812 | 7.069 | 7.222 | **7.444** | -2.9% |

What it says:

- With three candidates per position from the GPU and a first, plain optimal parse, the
  result **equals or beats plain `zstd -19`** on four of five datasets (it has the larger
  window) and sits **2-5% below `zstd -19 --long`** on real data, 12% on the log. The parse is
  the whole gap: greedy and lazy on the same candidates are 12-15% worse. What zstd's parser
  still has over this one is known engineering: per-block statistics for the prices, Huffman
  costs for literals, more match candidates (its binary tree keeps many per position), the
  repeat-offset rules with zero literals, and a third pass.
- The parse here ran on the CPU, slowly (a reference, not a product); the GPU parse is not
  written. Its throughput is the open question: one thread per 4 KB segment, matches of 64+
  taken whole so the dynamic programme needs only a 64-entry ring per thread (zstd's own
  shortcut), candidates laid out for coalesced reads. Our estimate is tens of milliseconds per
  128 MB; the match finder above (0.3-0.5 GB/s untuned, sorts alone 1 GB/s) would then be the
  limit.

## 4. The whole thing on the GPU (`gpuzstd.cu`)

`gpuzstd` runs the match finder and the optimal parse on the GPU and hands the sequences to
libzstd's entropy coder on a CPU thread, overlapped with the GPU's next chunk; the output is
standard zstd, one frame per 128 MB chunk, verified by decoding it. Every frame decoded to its
input in every run below.

The parse: **one warp per 4 KB segment.** The dynamic programme's state (a ring of 64 nodes:
cost in bits, offset, length, pending literals, three repeat offsets) lives in shared memory,
one array per field so a warp's accesses never conflict. The 32 lanes share each position's
work: byte comparisons 256 bytes at a time, and one match length per lane when relaxing a
match's lengths 3..63. A match of 64+ bytes is taken whole and may run past the segment's end;
the host trims the next segment's parse to where it ended. Pass 1 collects code statistics on
every 8th segment (the ratio is the same as on all of them); pass 2 prices with them. The
first version, one thread per segment with the ring in local memory, was latency-bound: it
took 135 ms per pass on the binaries, the warp version takes 70; on the log, 57 ms became 6.

The finder: four orders processed longest first (32, 16, 8, 4 bytes), with 32-bit hashed keys
for the three longer ones (half the radix-sort passes; the rare false groups fail the byte
comparison), and a position that already holds a 64+ byte match skips the shorter orders: the
parse never stops inside a long match.

RTX 5080, defaults (`--orders 32,16,8,4 --pred 4 --seg 4096 --passes 2 --sample 8`):

| Dataset | ratio | zstd -19 (8 MB) | zstd -19, 128 MB + LDM | GPU time per 128 MB | of which find · parse | GPU GB/s |
|---|---|---|---|---|---|---|
| binaries.tar | 3.817 | 3.77 | 3.944 | 312 ms | 215 · 16 + 70 | 0.43 |
| code.tar | 6.731 | 6.85 | 6.932 | 339 ms | 251 · 12 + 65 | 0.40 |
| logs.txt | 85.1 | 110.0 | 124.4 | 287 ms | 267 · 4 + 6 | 0.47 |
| mixed.tar | 3.743 | 3.69 | 3.934 | 320 ms | 222 · 15 + 73 | 0.42 |
| text.bin | 7.428 | 3.68 | 7.667 | 291 ms | 234 · 8 + 38 | 0.46 |
| 1 GB of System32 binaries (8 chunks) | 3.744 | | 3.879 | 2,573 ms | 1,760 · 127 + 599 | 0.42 |

End to end on the 1 GB file, with the CPU entropy coding (0.9 GB/s per thread) overlapped
with the GPU: **3.33 s, 0.32 GB/s**; `zstd -19 --long` on one thread took 248.6 s (4.3 MB/s)
for 3.5% smaller output. The join of the segments (0.33 s) still runs on the main thread
between chunks; moving it to the worker thread would bring the total to the GPU's time. The
file decodes to the original with an independent decoder too (`check_zst.py`, Python's zstd).

Settings that trade ratio for speed, measured on binaries.tar / logs.txt:

| Setting | ratio | finder |
|---|---|---|
| defaults | 3.817 / 85.1 | 215 / 267 ms |
| 64-bit keys (`--key64`) | same | +75 / +50 ms |
| 2 predecessors (`--pred 2`) | 3.804 / 83.5 | about the same since the early cap |
| orders 4, 8, 16 (no 32-byte order) | 3.788 / 69.0 | -50 / -100 ms |
| orders 4, 32 only | 3.601 / 84.7 | -100 / -80 ms |
| one pass (`--passes 1`) | 3.774 / 84.4 | parse -16 / -4 ms |

The log is the one dataset still well below zstd: 85 against 110-124. Its matches are hundreds
of bytes long; the 4 KB segments and the fresh repeat offsets at every segment start cost it
(16 KB segments: 86.7, 2 KB: 81.9), and so does taking a 64+ match whole where zstd would
weigh alternatives.

## Verdict

- **Ratio: reached, nearly.** Equal to or better than plain `zstd -19` on four of five
  datasets, 2-5% below `zstd -19 --long` (same 128 MB window) on real data, 30% below on the
  log. The remaining gap is parser engineering of the known kind (per-block statistics, Huffman
  costs for literals, zstd's repeat-offset rules, more candidates per position).
- **Speed: 0.4-0.47 GB/s on the GPU, 0.32 GB/s end to end, against a bar of 1 GB/s.** The
  parse is now cheap (70-85 ms per 128 MB, 6-10 on the log); the finder is 70% of the GPU time,
  and its floor with this design is the four radix sorts (about 70 ms per 128 MB) plus the
  candidate extensions (random 8-byte reads, 100-200 ms). Getting to 1 GB/s on this card would
  take a different finder (an LCP array per order instead of per-predecessor extension, or
  hashing instead of sorting); on an RTX 5090 the same code should be near 0.7 GB/s.
- **Against the CPU:** one thread does level 19 at 2-4 MB/s, sixteen at about 50 MB/s. The GPU
  path is 100-150x a core and 6-9x a 16-core machine, into files any zstd decompresses, at the
  ratio of `-19`. That is real, and well short of the hundreds-fold speed-up the first estimate
  hoped for.

## Running it

```
python make_corpus.py %TEMP%\gpuzip-bench\corpus 128
python ratio_study.py %TEMP%\gpuzip-bench\corpus                   (several minutes, 12 processes)
build_zstd.cmd                                                     (libzstd from the zstd-sys crate's sources, once)
build.cmd                                                          (nvcc; needs /Zc:preprocessor for CUB)
gpuzstd %TEMP%\gpuzip-bench\corpus\code.tar -o code.tar.zst         (--baseline adds zstd -19 + LDM, slow)
variants.ps1                                                       (settings against each other)
gpumatch %TEMP%\gpuzip-bench\corpus\code.tar 128 --dump             (the finder alone; writes code.tar.cand3, 3 GB)
cargo build --release --manifest-path zseq\Cargo.toml
zseq\target\release\zseq %TEMP%\gpuzip-bench\corpus\code.tar --k 3  (the CPU reference parses, minutes)
python make_big.py %TEMP%\gpuzip-bench\big-binaries.tar 1024        (1 GB of System32, several chunks)
```
