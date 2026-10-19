# How gpuzip works

Every entry in a ZIP file is compressed on its own, so nvCOMP decodes thousands of them at once
on the GPU. It gives one large entry, though, to a single GPU thread, at about 30 MB/s. One
large stream needs another approach: gpuinflate finds the block starts of a single Deflate
stream, decodes from all of them at once and stitches the pieces, at 3-5 GB/s on text
and 1-2 GB/s on data that hardly compresses (how, in [research/gpu-inflate](../research/gpu-inflate)).
So:

| Data | Where it is decoded |
|---|---|
| ZIP: Deflate, small | GPU with nvCOMP, in batches of up to 256 MB |
| ZIP: Deflate, large | CPU threads (libdeflate), or gpuinflate when that is faster (below) |
| ZIP: stored | CPU threads |
| ZIP: bzip2, LZMA, Zstandard, Deflate64, encrypted, PPMd | 7-Zip (`rescue` reads bzip2, LZMA, Zstandard and Deflate64 itself) |
| `.gz`, `.tar.gz`, `.tgz` | each member on gpuinflate (8 MB or more) or one CPU thread (zlib-rs) |
| Whole RAR, 7z, other tar.*, ... archives | 7-Zip |

In `auto` mode nvCOMP takes the smallest Deflate entries and the CPU threads take the largest,
until they meet. nvCOMP is started only when that pays: the CPU threads begin at once, and after
about 5 ms of their decoding gpuzip knows how fast they are on this archive and this machine.
The GPU joins only if it and the CPU threads together would finish sooner than the CPU threads
alone, counting its start-up (about 0.15 s), its speed on small entries (about 2.5 GB/s on an
RTX 5080) and the 0.05 s a loaded CUDA driver adds at exit. 16 Zen 5 threads decode the
benchmark archives at 6-8 GB/s and are done in about 0.2 s, so the GPU stays off; with 2 or 4
threads it is started and saves a third of the time.

gpuinflate takes, one at a time, the largest entries that it would finish before a CPU thread
could. It starts in about 0.15 s and decodes one stream of text at about 3.5 GB/s, but streams
that hardly compress (ROM images, data compressed before) only at about 1 GB/s, so its speed is
taken from each entry's compression ratio. A CPU thread decodes a large entry at about 0.6 GB/s
and takes one entry at a time, so with more large entries than threads the later ones wait
their turn. One 2 GB entry goes to gpuinflate; eight entries of 128 MB stay with 16 CPU threads,
but with 2 threads gpuinflate takes four of them. In `gpu` mode it takes every entry of 1 MB or
more. If gpuinflate reports a GPU error, the CPU threads take that entry and every large entry
after it at once, and gpuinflate is not used again in that run.

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

- The GPU runs in a thread of the same process. (The Python version gpuzip started as, removed
  after 0.1.1, needed a process of its own: nvCOMP's Python binding holds the GIL while it works,
  and a zlib thread beside it dropped from 1.48 to 0.09 GB/s.)
- Copies to and from the GPU run at about 6 GB/s from normal memory and about 28 GB/s from pinned
  memory, so the copies go through pinned buffers.
- nvCOMP's raw Deflate decoder reads output from 7-Zip, zlib, Python and .NET correctly
  (`tests/test_gpuzip.py` checks this).
- gpuzip calls the NVIDIA driver API (`nvcuda.dll`, `libcuda.so.1`) directly: nvCOMP
  links the CUDA runtime statically and works on the driver's primary context, so no CUDA
  runtime library is needed.

## Project layout

```
Cargo.toml      the program, gpuzip (Rust)
src/
  zip.rs        reads the archive's central directory and routes each entry
  dylib.rs      the libraries loaded at run time: their names on each system, where they are looked for
  gpu.rs        the NVIDIA driver and nvCOMP, loaded at run time
  extract.rs    shares the work between GPU, CPU threads and 7-Zip; writes the files
  outfile.rs    a file written gives it its name only once whole (no half file after a kill)
  sevenzip.rs   hands entries and whole archives to 7-Zip
  gz.rs         gzip members, trailers, tar extraction from a pipe
  inflate.rs    gpuinflate, loaded at run time
  rescue.rs     gzrescue, loaded at run time; tar extraction that skips destroyed headers
  ziprescue.rs  damaged ZIPs: local-header scan, per-entry recovery; Office XML repair and rebuild
  sheetfix.rs   damaged spreadsheets: sheets rebuilt from their valid cells, a lost workbook remade
  officefix.rs  damaged Word stories rebuilt, a presentation's lost master, layout and theme remade
  main.rs       the command line, reports and doctor
  ui.rs         terminal output: colors on a terminal only, a closed pipe ends quietly
tests/          correctness tests (ZIP, gzip, tar.gz, damaged files, zip slip)
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
