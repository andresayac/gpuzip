# How gpuzip works

Every entry in a ZIP file is compressed on its own, so nvCOMP decodes thousands of them at once
on the GPU. It gives one large entry, though, to a single GPU thread, at about 30 MB/s. One
large stream needs another approach: gpuinflate finds the block starts of a single Deflate
stream, decodes from all of them at once and stitches the pieces, at 3-5 GB/s (how, in
[research/gpu-inflate](../research/gpu-inflate)). So:

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

## Implementation notes

Learned the hard way:

- The nvCOMP Python binding holds the GIL while it works, which stalls every other Python
  thread (a zlib thread dropped from 1.48 to 0.09 GB/s). The Python version therefore runs the
  GPU in its own process and hands results back through shared memory. The Rust version has no
  such lock and keeps the GPU in a thread of the same process.
- Copies to and from the GPU run at about 6 GB/s from normal memory and about 28 GB/s from pinned
  memory, so both versions copy through pinned buffers.
- nvCOMP's raw Deflate decoder reads output from 7-Zip, zlib, Python and .NET correctly
  (`tests/test_gpuzip.py` checks this, against both versions).
- The Rust version calls the NVIDIA driver API (`nvcuda.dll`, `libcuda.so.1`) directly: nvCOMP
  links the CUDA runtime statically and works on the driver's primary context, so no CUDA
  runtime library is needed.

## Project layout

```
rust/           the Rust version
  src/zip.rs        reads the archive's central directory and routes each entry
  src/dylib.rs      the libraries loaded at run time: their names on each system, where they are looked for
  src/gpu.rs        the NVIDIA driver and nvCOMP, loaded at run time
  src/extract.rs    shares the work between GPU, CPU threads and 7-Zip; writes the files
  src/sevenzip.rs   hands entries and whole archives to 7-Zip
  src/gz.rs         gzip members, trailers, tar extraction from a pipe
  src/inflate.rs    gpuinflate, loaded at run time
  src/rescue.rs     gzrescue, loaded at run time; tar extraction that skips destroyed headers
  src/ziprescue.rs  damaged ZIPs: local-header scan, per-entry recovery; Office XML repair and rebuild
  src/sheetfix.rs   damaged spreadsheets: sheets rebuilt from their valid cells, a lost workbook remade
  src/officefix.rs  damaged Word stories rebuilt, a presentation's lost master, layout and theme remade
  src/main.rs       the command line, reports and doctor
  src/ui.rs         terminal output: colors on a terminal only, a closed pipe ends quietly
python/         the Python version (gpuzip.py, pyproject.toml)
tests/          correctness tests for both versions (ZIP, gzip, tar.gz, damaged files, zip slip)
gpuinflate/     the library that decodes one Deflate stream on the GPU, in segments (C/CUDA),
                with its tests and fuzzer; rescue.cpp, the recovery of damaged gzip files (CPU);
                build.cmd (Windows) and build.sh (Linux, macOS) build both
bench/          the benchmarks, their latest results (results*.json, results*.md) and charts.py,
                which draws the charts in docs/img from them
docs/           this documentation
research/gpu-inflate/   the experiment gpuinflate grew from: the algorithm, measurements and
                        dead ends, 4-18x faster than rapidgzip on 16 threads (see its README)
research/gpu-zstd/      feasibility study of a GPU compressor writing standard zstd at level-19
                        ratio: match finding by radix sorts and an optimal parse on the GPU,
                        0.3-0.5 GB/s on an RTX 5080, 75x one CPU thread (see its README)
research/recovery/      gzrescue: recovering damaged .gz files block by block with the finder;
                        98-99.8% back on ordinary data where gzip gives 30-60%; eleven real-world
                        ways files break, on six formats, against gzip, tar, 7-Zip (see its README)
```
