# gpuzip

Extract ZIP and gzip archives on an NVIDIA GPU, with automatic CPU fallback. Small ZIP entries
go to [nvCOMP](https://docs.nvidia.com/cuda/nvcomp/) in batches; a single large stream (a
`.gz`, a `.tar.gz`, a large ZIP entry) goes to [gpuinflate](gpuinflate), our own decoder that
splits one Deflate stream across thousands of GPU threads. Other formats (RAR, 7z, tar.xz, ...)
are handed to 7-Zip. Prototype.

Two implementations with the same commands, options and JSON reports:

- **Rust** (`rust/`): the faster one. One `.exe`, starts in milliseconds, decodes 13-20x faster
  than 7-Zip. Loads the NVIDIA driver, nvCOMP and gpuinflate at run time, so it also runs
  without them. Reads `.gz` and `.tar.gz` itself.
- **Python** (`python/`): the first version, kept as the reference the Rust one is tested
  and benchmarked against. Hands `.gz` and `.tar.gz` to 7-Zip.

```
gpuzip x photos.zip               extract into .\photos
gpuzip x *.zip -o D:\out          extract each archive into D:\out\<name>
gpuzip x logs.tar.gz              extract the tar inside into .\logs
gpuzip x dump.sql.gz              decompress to .\dump.sql, like gunzip
gpuzip x backup.rar               RAR, 7z and other tar.* go to 7-Zip
gpuzip t backup.zip               check every file's CRC, write nothing
gpuzip l backup.zip               list entries and where each is decoded
gpuzip doctor                     check the GPU, nvCOMP and 7-Zip
```

`x`, `t` and `l` also answer to `extract`, `test` and `list`. Options:

| Option | What it does |
|---|---|
| `-o DIR` | output folder (default: the archive's name, in the current folder) |
| `--mode auto\|gpu\|cpu` | `auto` (default): GPU and CPU share the work, each where it is faster · `gpu`: all Deflate on the GPU · `cpu`: no GPU |
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

The program is `rust\target\release\gpuzip.exe`. The GPU paths need two DLLs next to it, each
optional (without one, that work goes to the CPU):

- `nvcomp64_6.dll`, for small ZIP entries (or point `GPUZIP_NVCOMP_DIR` at its folder). It
  comes with `pip install nvidia-libnvcomp-cu13` (in `site-packages\nvidia\libnvcomp\bin`) or
  NVIDIA's nvCOMP download.
- `gpuinflate.dll`, for `.gz`, `.tar.gz` and large ZIP entries (or point `GPUZIP_INFLATE_DIR`
  at its folder). Build it with `gpuinflate\build.cmd release`, which needs the CUDA toolkit
  (see [gpuinflate/README.md](gpuinflate/README.md)).

Running needs no CUDA toolkit: the NVIDIA driver is enough. `gpuzip doctor` shows what was found.

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

Every entry in a ZIP file is compressed on its own, so nvCOMP decodes thousands of them at once
on the GPU. It gives one large entry, though, to a single GPU thread, at about 30 MB/s. One
large stream needs another approach: gpuinflate finds the block starts of a single Deflate
stream, decodes from all of them at once and stitches the pieces, at 3-5 GB/s (how, in
[research/gpu-inflate](research/gpu-inflate)). So:

| Data | Where it is decoded |
|---|---|
| ZIP: Deflate, small | GPU with nvCOMP, in batches of up to 256 MB |
| ZIP: Deflate, large | CPU threads (Rust: libdeflate · Python: zlib-ng), or in Rust gpuinflate when that is faster (below) |
| ZIP: stored | CPU threads |
| ZIP: bzip2, LZMA, Zstandard | Python: CPU threads · Rust: 7-Zip |
| ZIP: encrypted, Deflate64, PPMd | 7-Zip |
| `.gz`, `.tar.gz`, `.tgz` | Rust: each member on gpuinflate (8 MB or more) or one CPU thread (zlib-rs) · Python: 7-Zip |
| Whole RAR, 7z, other tar.*, ... archives | 7-Zip |

In `auto` mode nvCOMP takes the smallest Deflate entries and the CPU threads take the largest,
until they meet. gpuinflate takes, one at a time, the largest entries that it would finish
before a CPU thread could (it runs at about 4 GB/s on one stream after starting in about
0.15 s, a CPU thread at about 1 GB/s): one 2 GB entry goes to it, eight entries of 128 MB stay
with the 16 CPU threads. In `gpu` mode it takes every entry of 1 MB or more.

gzip files are read member by member: header checks, the Deflate stream, then the trailer's
CRC32 and size. A `.tar.gz` is extracted by the [tar](https://crates.io/crates/tar) crate while
it is being decompressed, from a pipe, so nothing is held whole in memory; entries whose path
would leave the output folder are refused. A plain `.gz` becomes one file in the output folder
(the current folder by default), named like gunzip names it.

Every file's CRC32 is checked; a GPU result that fails the check, or a GPU error, means the
work is done again on the CPU. Entries whose path would leave the output folder ("zip slip")
are refused.

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
- With nvCOMP the GPU never beats the CPU on these ZIP files: it decodes small entries at
  several GB/s once running, but 8 Zen 5 cores with libdeflate are faster still, and it gives
  a large entry to one GPU thread (8 files of 128 MB: 21-42 s on the GPU, 0.27 s on the CPU).
  These numbers predate gpuinflate; with it, `--mode gpu` tests those 8 files in 0.43 s, and
  `auto` rightly leaves them to the CPU threads. A ZIP with one 2.1 GB entry tests in 0.70 s
  with gpuinflate (`auto` picks it) and 3.84 s on the CPU.

Where the GPU could still pay off for ZIP: decoding straight into GPU memory for
machine-learning data loading (no files written, no copy back), machines with a weak CPU, and
NVIDIA's data-center Blackwell GPUs, whose hardware decompression engine handles Deflate.

### gzip

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

## Layout

```
rust/           the Rust version
  src/zip.rs        reads the archive's central directory and routes each entry
  src/gpu.rs        the NVIDIA driver and nvCOMP, loaded at run time
  src/extract.rs    shares the work between GPU, CPU threads and 7-Zip; writes the files
  src/sevenzip.rs   hands entries and whole archives to 7-Zip
  src/gz.rs         gzip members, trailers, tar extraction from a pipe
  src/inflate.rs    gpuinflate.dll, loaded at run time
  src/main.rs       the command line, reports and doctor
python/         the Python version (gpuzip.py, pyproject.toml)
tests/          correctness tests for both versions (ZIP, gzip, tar.gz, damaged files, zip slip)
gpuinflate/     the library that decodes one Deflate stream on the GPU, in segments (C/CUDA),
                with its tests and fuzzer
bench/          the benchmarks and their latest results (results*.md, results*.json)
research/gpu-inflate/   the experiment gpuinflate grew from: the algorithm, measurements and
                        dead ends, 4-18x faster than rapidgzip on 16 threads (see its README)
research/gpu-zstd/      feasibility study of a GPU compressor writing standard zstd at level-19
                        ratio: match finding by radix sorts and an optimal parse on the GPU,
                        0.3-0.5 GB/s on an RTX 5080, 75x one CPU thread (see its README)
```

Tests: `python tests\test_gpuzip.py` checks the Python version; with
`set GPUZIP_BIN=rust\target\release\gpuzip.exe` first, it checks the Rust build. They pack a
folder with 7-Zip, Python and PowerShell, extract it in every mode and compare every file.
