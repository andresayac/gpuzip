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

| Dataset | Job | 7-Zip | Rust CPU | Rust GPU | Rust auto |
|---|---|---|---|---|---|
| 40,000 small files, 1.1 GB | test | 4.34 s | **0.26 s** | 0.57 s | **0.26 s** |
| | extract | 25.54 s | 18.80 s* | - | 19.36 s* |
| 2,000 medium files, 1.4 GB | test | 5.02 s | **0.26 s** | 2.36 s | **0.26 s** |
| | extract | 7.12 s | 1.48 s* | - | 1.58 s* |
| 8 large files, 1.1 GB | test | 3.63 s | **0.26 s** | 0.48 s | **0.26 s** |
| | extract | 3.90 s | **0.48 s** | - | **0.48 s** |
| Mixed, 10,304 files, 0.8 GB | test | 2.83 s | 0.22 s | 0.85 s | **0.20 s** |
| | extract | 9.59 s | 5.41 s* | - | 5.29 s* |

\* Windows Defender inspects every file the Rust build writes, at about 0.35 ms a file, far more
than it charges Python's signed interpreter. The same `gpuzip.exe` run from `%TEMP%` (which this
machine's Defender apparently leaves alone) extracts much faster:

| Extract | 7-Zip | Rust CPU, from the project folder | Rust CPU, from `%TEMP%` |
|---|---|---|---|
| 40,000 small files | 26.38 s | 19.50 s | **6.08 s** |
| 2,000 medium files | 7.25 s | 1.81 s | **1.22 s** |
| Mixed, 10,304 files | 8.79 s | 4.82 s | **1.74 s** |

What this shows:

- Decoding: gpuzip is 13-20x faster than 7-Zip. 7-Zip decodes ZIP
  at 0.2-0.3 GB/s here; gpuzip with libdeflate at up to 5.6 GB/s, process start included.
- Extracting many files is limited by creating them, and on Windows by how Defender treats
  the program. A code-signed release with some reputation should behave like the `%TEMP%` column.
- With nvCOMP the GPU does not beat 16 CPU threads on these ZIP files. Once running it decodes
  small entries at about 2.5-3 GB/s; 8 Zen 5 cores with libdeflate reach 6-8 GB/s, and the
  GPU's start-up (about 0.15 s, and about 0.05 s more to unload the driver at exit) is most of
  the CPU's whole time. So `auto` times the CPU threads on their first entries and leaves the
  GPU off here, and matches `--mode cpu`. It used to start nvCOMP anyway and was 19-36% slower
  on the small, medium and mixed files (0.39, 0.35 and 0.22 s against 0.28, 0.26 and 0.19 s).
- On a weaker CPU the GPU does pay, and `auto` uses it. `--threads N` stands in for a smaller CPU
  with the same GPU (test, median of 9, process start included):

  | CPU threads | Data | `--mode cpu` | `auto` | GPU in `auto` |
  |---|---|---|---|---|
  | 2 | small | 0.92 s | **0.50 s** | yes |
  | 2 | medium | 1.11 s | **0.70 s** | yes |
  | 2 | mixed | 0.63 s | **0.47 s** | yes |
  | 4 | small | 0.56 s | **0.49 s** | yes |
  | 4 | medium | 0.64 s | **0.55 s** | yes |
  | 4 | mixed | 0.35 s | 0.35 s | no (with it: 0.31-0.41 s) |

- Large entries go to gpuinflate when it would finish them before a CPU thread, counting its
  speed by how well each entry compresses and the CPU threads' turns. A 5.8 GB Android ROM ZIP
  (54 entries; nine parts of about 750 MB, compressed only to 0.77-0.91 of their size, which
  gpuinflate decodes at 1.1-1.7 GB/s) and the 8 large files, test, median of 5-7:

  | Archive | CPU threads | `--mode cpu` | `auto` | gpuinflate in `auto` |
  |---|---|---|---|---|
  | ROM, 7.4 GB | 16 | 2.80 s | **2.48 s** | 2 entries |
  | | 4 | 4.47 s | **3.31 s** | 3 entries |
  | | 2 | 6.95 s | **4.29 s** | 4 entries |
  | 8 large files | 16 | 0.29 s | 0.29 s | none |
  | | 2 | 0.81 s | **0.49 s** | 4 entries |

- `--mode gpu` gives every entry of 1 MB or more to gpuinflate, one at a time, and the rest to
  nvCOMP: 0.48 s for the 8 files of 128 MB (nvCOMP alone gives a large entry to one GPU thread:
  43 s in 0.1's Python version, which had no gpuinflate), but 2.36 s for 2,000 files of up to
  2 MB. A ZIP with one 2.1 GB entry tests in 0.70 s with gpuinflate (`auto` picks it) and
  3.84 s on the CPU.

Where the GPU could still pay off for ZIP: machines with a weak CPU (above), decoding straight
into GPU memory for machine-learning data loading (no files written, no copy back), and
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
