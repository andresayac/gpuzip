# Benchmarks

## ZIP

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="img/zip-speed-dark.svg">
  <img alt="Testing ZIP archives: 7-Zip against gpuzip on the CPU" src="img/zip-speed-light.svg">
</picture>

`python bench\bench.py --data <folder>` builds four datasets, packs them with 7-Zip (zip, Deflate,
`-mx5`), and times each tool in interleaved rounds (median of 3, warm file cache, process start
included). RTX 5080, Ryzen 7 9700X (8 cores / 16 threads), NVMe SSD, Windows 11 with Defender
on, 7-Zip 24.09, nvCOMP 6.0. The Rust version's extracted files were identical to 7-Zip's on
every dataset.

| Dataset | Job | 7-Zip | Python CPU | Python GPU | Python auto | Rust CPU | Rust GPU | Rust auto |
|---|---|---|---|---|---|---|---|---|
| 40,000 small files, 1.1 GB | test | 4.78 s | 0.87 s | 1.79 s | 1.00 s | **0.28 s** | 0.61 s | 0.39 s |
| | extract | 26.81 s | **9.86 s** | - | 11.20 s | 19.56 s* | - | 19.77 s* |
| 2,000 medium files, 1.4 GB | test | 5.08 s | 0.46 s | 2.03 s | 0.48 s | **0.26 s** | 0.86 s | 0.35 s |
| | extract | 7.31 s | **1.08 s** | - | 1.53 s | 1.56 s* | - | 1.87 s* |
| 8 large files, 1.1 GB | test | 3.66 s | 0.45 s | 42.23 s | 0.45 s | **0.27 s** | 21.16 s | **0.27 s** |
| | extract | 4.01 s | 0.67 s | - | 0.66 s | **0.62 s** | - | 1.45 s |
| Mixed, 10,304 files, 0.8 GB | test | 2.80 s | 0.46 s | 11.93 s | 0.51 s | **0.19 s** | 5.77 s | 0.22 s |
| | extract | 9.07 s | **2.89 s** | - | 3.24 s | 5.38 s* | - | 5.21 s* |

\* Windows Defender inspects every file the Rust build writes, at about 0.35 ms a file, far more
than it charges Python's signed interpreter. The same `gpuzip.exe` run from `%TEMP%` (which this
machine's Defender apparently leaves alone) extracts much faster:

| Extract | 7-Zip | Python CPU | Rust CPU, from the project folder | Rust CPU, from `%TEMP%` |
|---|---|---|---|---|
| 40,000 small files | 26.38 s | 9.44 s | 19.50 s | **6.08 s** |
| 2,000 medium files | 7.25 s | 1.41 s | 1.81 s | **1.22 s** |
| Mixed, 10,304 files | 8.79 s | 2.77 s | 4.82 s | **1.74 s** |

What this shows:

- Decoding: Rust is 13-20x faster than 7-Zip and 1.7-3x faster than Python. 7-Zip decodes ZIP
  at 0.2-0.3 GB/s here; Rust with libdeflate at up to 5.6 GB/s, process start included.
- Extracting many files is limited by creating them, and on Windows by how Defender treats
  the program. A code-signed release with some reputation should behave like the `%TEMP%` column.
- With nvCOMP the GPU never beats the CPU on these ZIP files: it decodes small entries at
  several GB/s once running, but 8 Zen 5 cores with libdeflate are faster still, and it gives
  a large entry to one GPU thread (8 files of 128 MB: 21-42 s on the GPU, 0.27 s on the CPU).
  These numbers predate gpuinflate; with it, `--mode gpu` tests those 8 files in 0.43 s, and
  `auto` rightly leaves them to the CPU threads. A ZIP with one 2.1 GB entry tests in 0.70 s
  with gpuinflate (`auto` picks it) and 3.84 s on the CPU.

Where the GPU could still pay off for ZIP: decoding straight into GPU memory for
machine-learning data loading (no files written, no copy back), machines with a weak CPU, and
NVIDIA's data-center Blackwell GPUs, whose hardware decompression engine handles Deflate.

## gzip

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="img/gzip-speed-dark.svg">
  <img alt="Testing gzip files: GNU gzip, 7-Zip, rapidgzip and gpuzip" src="img/gzip-speed-light.svg">
</picture>

`python bench\bench_gz.py --data <folder>`: same rules (median of 3, warm cache, process start
included), same machine; rapidgzip 0.16 with 16 threads, GNU gzip 1.13 from Git for Windows.
Every extraction was checked against the expected bytes or 7-Zip's extraction.

| Data | Job | 7-Zip | GNU gzip | rapidgzip | Windows tar | gpuzip cpu | gpuzip auto (GPU) |
|---|---|---|---|---|---|---|---|
| 128 MB of text (7-Zip -mx5) | test | 0.47 s | 0.62 s | **0.15 s** | - | 0.24 s | 0.19 s |
| | extract | 0.51 s | 0.68 s | **0.17 s** | - | 0.28 s | 0.22 s |
| 6.4 GB of text (zlib level 4) | test | 21.22 s | 28.50 s | 3.34 s | - | 10.63 s | **2.24 s** |
| tar.gz of 2,000 files, 1.4 GB | test | 4.67 s | 6.25 s | 0.82 s | - | 2.45 s | **0.70 s** |
| | extract | 7.08 s* | - | 2.87 s** | 3.87 s | 2.83 s | **2.69 s** |

\* 7-Zip decompresses to a pipe and a second 7-Zip unpacks the tar from it. \*\* rapidgzip
piped into Windows tar.

What this shows:

- On large streams gpuinflate is the fastest decoder here: 9.5x 7-Zip and 1.5x rapidgzip on
  16 threads for 6.4 GB (2.9 GB/s, process start, CUDA set-up and CRC included).
- Small files pay a fixed cost: about 80 ms to create the CUDA context, plus set-up, so on
  128 MB rapidgzip still wins (0.15 s against 0.19 s).
- Extracting a tar.gz of many files is limited by writing them (decoding takes about 0.7 s of
  the 2.7 s); the tar extractor writes one file at a time.

The charts are drawn from `bench/results.json` and `bench/results-gz.json` by `python bench/charts.py`.
