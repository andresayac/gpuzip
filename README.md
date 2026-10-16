# gpuzip

Extract ZIP archives on an NVIDIA GPU with [nvCOMP](https://docs.nvidia.com/cuda/nvcomp/), with
automatic CPU fallback. Other formats (RAR, 7z, tar.gz, ...) are handed to 7-Zip. Prototype.

Two implementations with the same commands, options and JSON reports:

- **Rust** (`rust/`): the faster one. One `.exe`, starts in milliseconds, decodes 13-20x faster
  than 7-Zip. Loads the NVIDIA driver and nvCOMP at run time, so it also runs without them.
- **Python** (`python/`): the first version, kept as the reference the Rust one is tested
  and benchmarked against.

```
gpuzip x photos.zip               extract into .\photos
gpuzip x *.zip -o D:\out          extract each archive into D:\out\<name>
gpuzip x backup.rar               RAR, 7z and tar.* go to 7-Zip
gpuzip t backup.zip               check every file's CRC, write nothing
gpuzip l backup.zip               list entries and where each is decoded
gpuzip doctor                     check the GPU, nvCOMP and 7-Zip
```

`x`, `t` and `l` also answer to `extract`, `test` and `list`. Options:

| Option | What it does |
|---|---|
| `-o DIR` | output folder (default: the archive's name, in the current folder) |
| `--mode auto\|gpu\|cpu` | `auto` (default): GPU and CPU share the work · `gpu`: all Deflate on the GPU · `cpu`: no GPU |
| `--password PW` | password for encrypted entries (passed to 7-Zip) |
| `-v`, `-q` | more detail (GPU timings, why work went where) · print only failures |
| `--json` | one JSON report per archive, for scripts |
| `--threads N` | CPU decode threads (default: all) |

Exit codes: 0 ok · 1 missing file or setup problem · 2 some entries failed.

## Install

Both need [7-Zip](https://www.7-zip.org) for RAR, 7z, tar.* and encrypted entries. The GPU path
needs an NVIDIA GPU and driver; without them every entry is decoded on the CPU.

### Rust

Needs [Rust](https://rustup.rs) and the Visual Studio Build Tools (C++). From this folder:

```
cargo build --release --manifest-path rust\Cargo.toml
```

The program is `rust\target\release\gpuzip.exe`. For the GPU path, put `nvcomp64_6.dll` next
to it (or point `GPUZIP_NVCOMP_DIR` at its folder). It comes with `pip install
nvidia-libnvcomp-cu13` (in `site-packages\nvidia\libnvcomp\bin`) or NVIDIA's nvCOMP download.
No CUDA toolkit is needed: the NVIDIA driver is enough. `gpuzip doctor` shows what was found.

### Python

Needs Python 3.10+ (3.14 for Zstandard entries). From this folder, as a command available
everywhere:

```
uv tool install "./python[gpu]"
```

or into a virtual environment (`pip install "./python[gpu]"`, or `pip install ./python` for
CPU only).

Both install a command called `gpuzip`; if both are on your PATH, the first one found wins.

## How it works

Every entry in a ZIP file is compressed on its own, so the GPU can decode thousands of them at
once. One large entry, though, is decoded by a single GPU thread, at about 30 MB/s. So:

| Entry | Where it is decoded |
|---|---|
| Deflate, small | GPU, in batches of up to 256 MB |
| Deflate, large (over 1 MB) | CPU threads (Rust: libdeflate · Python: zlib-ng) |
| Stored | CPU threads |
| bzip2, LZMA, Zstandard | Python: CPU threads · Rust: 7-Zip |
| Encrypted, Deflate64, PPMd | 7-Zip |
| Whole RAR, 7z, tar.*, ... archives | 7-Zip |

In `auto` mode the GPU takes the smallest Deflate entries and the CPU threads take the largest,
until they meet. Every file's CRC32 is checked; a GPU result that fails the check is decoded
again on the CPU. Entries whose path would leave the output folder ("zip slip") are refused.

Implementation notes learned the hard way:

- The nvCOMP Python binding holds the GIL while it works, which stalls every other Python
  thread (a zlib thread dropped from 1.48 to 0.09 GB/s). The Python version therefore runs the
  GPU in its own process and hands results back through shared memory. The Rust version has no
  such lock and keeps the GPU in a thread of the same process.
- Copies to and from the GPU run at about 6 GB/s from normal memory and about 28 GB/s from pinned
  memory, so both versions copy through pinned buffers.
- nvCOMP's raw Deflate decoder reads output from 7-Zip, zlib, Python and .NET correctly
  (`tests/test_gpuzip.py` checks this, against both versions).
- The Rust version calls the NVIDIA driver API (`nvcuda.dll`) directly: nvCOMP links the CUDA
  runtime statically and works on the driver's primary context, so no CUDA runtime DLL is needed.

## Benchmark

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
- On this machine the GPU never beats the CPU: it decodes small entries at several GB/s once
  running, but 8 Zen 5 cores with libdeflate are faster still. Large entries are a poor fit
  (8 files of 128 MB: 21-42 s on the GPU, 0.27 s on the CPU).

Where the GPU could still pay off: decoding straight into GPU memory for machine-learning data
loading (no files written, no copy back), machines with a weak CPU, and NVIDIA's data-center
Blackwell GPUs, whose hardware decompression engine handles Deflate.

## Layout

```
rust/           the Rust version
  src/zip.rs        reads the archive's central directory and routes each entry
  src/gpu.rs        the NVIDIA driver and nvCOMP, loaded at run time
  src/extract.rs    shares the work between GPU, CPU threads and 7-Zip; writes the files
  src/sevenzip.rs   hands entries and whole archives to 7-Zip
  src/main.rs       the command line, reports and doctor
python/         the Python version (gpuzip.py, pyproject.toml)
tests/          correctness tests for both versions
bench/          the benchmark and its latest results (results.md, results.json)
```

Tests: `python tests\test_gpuzip.py` checks the Python version; with
`set GPUZIP_BIN=rust\target\release\gpuzip.exe` first, it checks the Rust build. They pack a
folder with 7-Zip, Python and PowerShell, extract it in every mode and compare every file.
