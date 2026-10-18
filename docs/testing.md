# Testing gpuzip on real archives

`tests/test_gpuzip.py` checks gpuzip on archives made for it. This page is about everything
else: archives from the wild and from other tools, hostile ones, damage as it happens, other
file systems, a busy GPU, and the tools people already use. The first campaign ran on
2026-10-17, against v0.1.1 and two branches, on an RTX 5080, a Ryzen 7 9700X, Windows 11 and
Ubuntu 24.04 in WSL2. What it found, and what was done about it, is at the end.

## The scripts

| Script | What it does |
|---|---|
| `bench/make_realworld.py DIR` | makes the archives other tools and odd situations make: 7-Zip at -mx9, AES-256, ZipCrypto, Deflate64, bzip2, LZMA; Python's stored, Zstandard and streamed (data descriptors) ZIPs; PowerShell, Info-ZIP, a JAR; Zip64 by count (70,000 entries) and by size (4.5 GB); a self-extractor's stub before the archive; a 60 KB comment; names Windows refuses; entries that try to leave the folder; gzip with three members, every header field, bgzip's layout; a tar.gz with long names and links |
| `bench/realworld.py --bin A=PATH --bin B=PATH PATHS...` | every archive, every gpuzip given, every mode: `x` and `t`, every extracted file compared by SHA-256 with 7-Zip's extraction (Python's zipfile, or the source folder, when 7-Zip itself fails) |
| `bench/realworld.py --rescue ...` | each ZIP and tar.gz damaged in the eleven ways of `research/recovery/realistic.py`, then `gpuzip rescue`: files back identical, files called intact, and silent errors (called intact, but wrong) |
| `bench/malicious.py --bin PATH` | hostile archives: overlapping entries (a zip bomb), sizes that lie both ways, a wrong CRC, offsets past the end, tar links out of the folder, gzip bombs extracted and rescued with peak memory measured |
| `bench/fuzz_archives.py --bin PATH` | 3,000 randomly damaged ZIPs (four producers, Zip64, data descriptors), tar.gz and gzip files; each tested, extracted and rescued: no crash, no hang, nothing written outside, `t` and `x` agreeing |
| `bench/make_edge.py --bin PATH` | names from Japanese, Chinese and Russian Windows; split ZIPs, 7-Zip volumes, a self-extractor; passwords wrong, missing, mixed; Unix permissions, read-only, folder times, extended time |
| `bench/stress.py --bin PATH ...` | the GPU's memory nearly full, four gpuzip at once, a run killed and repeated, an output folder it may not write to, missing, empty, truncated and non-archive inputs |
| `bench/soak.py --bin PATH ...` | 200 runs in a row (time, GPU memory), the same with another process keeping the GPU busy, an extraction stopped by Ctrl+C and by a kill |
| `bench/compare.py --gpuzip PATH ...` | time to extract against 7-Zip, ripunzip, bsdtar, Expand-Archive, File Explorer, Info-ZIP's unzip, GNU gzip, pigz, rapidgzip, with every tool's output the same size |

The public archives used: `linux-6.6.tar.gz` (kernel.org), `Python-3.14.0.tgz` (python.org),
`enwik9.zip` (mattmahoney.net), `silesia.zip` (sun.aei.polsl.pl), and CPython v3.13.0 as a
GitHub zip; plus an Android ROM (5.8 GB, 54 entries, nine of them about 740 MB that compress to
0.77-0.91) and a 12 GB RAR.

## What held

- **Every byte right.** On 33 archives from 14 producers, in three modes, two builds and two
  systems, every file gpuzip extracted matched 7-Zip's, except where 7-Zip itself was wrong: a
  ZIP whose local names differ from its directory's (7-Zip wrote 66 empty files, gpuzip the right
  ones) and one with a self-extractor's stub before it (7-Zip opened nothing).
- **Fuzzing:** 3,000 damaged archives, 9,000 runs: no crash, no hang, nothing written outside the
  output folder, `t` and `x` always agreeing.
- **Hostile input:** sizes that lie (1 KB declared for 100 MB, 4 GB or 1 TB declared for 1 KB),
  wrong CRCs and offsets past the end are refused without large allocations; `../`, `..\`,
  absolute paths and drive letters stay inside the folder; on Linux a tar symbolic link out of
  the folder, then a file written through it, is refused ("outside of destination path").
- **Under pressure:** with another process holding 14.7 of 16.3 GB of GPU memory, four gpuzip at
  once, 200 runs in a row (times flat, GPU memory back to the same level), a GPU busy with matrix
  products (1.7x slower, still right), an output folder on another disk and on a network path.
- **Damage:** the 11 realistic failures on silesia, a GitHub zip, Python's and the kernel's
  tarballs, three generated ZIPs (up to 70,000 entries): no silent errors except the CR LF case
  below.

## Against the tools people use

`bench/compare.py`, median of three extractions into an empty folder, process start included,
every tool's output the same size.

Linux (WSL2, ext4), gpuzip v0.1.1:

| Archive | gpuzip | gpuzip, CPU | 7-Zip | ripunzip | Info-ZIP unzip | other |
|---|---|---|---|---|---|---|
| ZIP of 4,882 source files (GitHub) | 0.23 s* | **0.05 s** | 0.40 s | 0.11 s | 0.50 s | |
| enwik9.zip, one 1 GB entry | **0.63 s** | 1.51 s | 3.37 s | 1.71 s | 4.39 s | |
| linux-6.6.tar.gz, 81,703 files | 2.18 s | **1.75 s** | 5.44 s | | | GNU tar 4.84 s, tar with pigz 3.69 s |
| a 6.4 GB .gz | **3.43 s** | 11.65 s | 25.11 s | | | GNU gzip 31.68 s, pigz 23.32 s |

\* v0.1.1's auto mode sends this ZIP's small entries to nvCOMP; the auto-mode branch (finding 15)
leaves them to the CPU, as on Windows below.

Windows 11 (NTFS, Defender on), gpuzip from the auto-mode branch:

| Archive | gpuzip | gpuzip, CPU | 7-Zip | ripunzip | bsdtar (tar.exe) | Expand-Archive | File Explorer |
|---|---|---|---|---|---|---|---|
| ZIP of 4,882 source files | **0.77 s** | 0.83 s | 4.42 s | 1.44 s | 1.94 s | 56.9 s | 41.0 s |
| ZIP of 70,000 small files | 9.01 s | 9.02 s | 48.0 s | **7.99 s** | 38.4 s | 728.8 s | 480.2 s |
| enwik9.zip, one 1 GB entry | **0.61 s** | 1.74 s | 3.37 s | 2.42 s | 1.39 s | 5.49 s | 5.86 s |
| Android ROM, 5.8 GB, 54 entries | **3.31 s** | 4.40 s | 25.9 s | 7.20 s | 15.2 s | 44.6 s | 50.6 s |
| a 6.4 GB .gz | **3.24 s** | 12.26 s | 24.25 s | | | | rapidgzip 4.26 s, GNU gzip 31.76 s |

Where gpuzip loses: ripunzip writes 70,000 tiny files faster on Windows (finding 17), and on
Linux a ZIP of many small files is faster on the CPU than with the GPU's start-up (finding 15).

## What it found

| | Finding | Where | State |
|---|---|---|---|
| 1 | gpuinflate ran past its input buffer on large streams that compress less than about 2.4:1: every such entry failed on the GPU and was redone on the CPU | the Android ROM | fixed in 0.1.1 |
| 2 | A tar.gz whose CR LF pairs became LF: files called intact are wrong (Python's tarball: 114 of 163; the kernel's: 5,732 of 8,092) | rescue | fixed on a branch, checked here: with no checksum to confirm them such files are "unverified", never "intact"; 0 silent errors on every case above |
| 3 | Rescue holds 6-8 times the output in memory (2 GB out: 12-17 GB), so a large damaged file, or a small gzip bomb, exhausts RAM | rescue | fixed on a branch (a 2 GB .gz: 24.8 GB to 1.1 GB, 80 s to 37 s), to check here once combined with finding 2's |
| 4 | A branch that placed damage inside blocks called 53 wrong kernel files intact | rescue branch | fixed before merging (d0f67aa) |
| 5 | Searching for lost CRs in a binary ZIP takes 2 minutes and finds none (silesia) | rescue | to bound in time |
| 6 | An entry named `nul` (or `CON`, `PRN`, `AUX`, `COM1`...) is written to the Windows device and lost, with no error | extraction, Windows | to fix |
| 7 | tar symbolic links fail on Windows ("failed to unpack", exit 2: 63 in the kernel); 7-Zip writes empty files instead | extraction, Windows | to fix: link when allowed, else a copy of the target, with a note |
| 8 | tar entries that differ only in case overwrite each other with no note on Windows and macOS (13 pairs in the kernel); ZIP extraction does note it | extraction | to fix |
| 9 | Overlapping ZIP entries are not detected: 100 KB extract to 2 GB | extraction | to fix (Info-ZIP's unzip refuses them) |
| 10 | The Unix executable bit is not restored from ZIP entries (it is from tar) | extraction, Linux, macOS | to fix |
| 11 | Names from Japanese, Chinese or Russian Windows come out garbled; no way to name the code page (7-Zip has -mcp) | extraction | to add |
| 12 | An interrupted extraction leaves partial files under their final names | extraction | to fix: write under a temporary name |
| 13 | Info-ZIP's extended time is ignored (the local DOS time is used); folder times and the read-only attribute are not restored | extraction | to fix |
| 14 | `--mode gpu` on a gzip of thousands of small members (bgzip) takes 35 s against 0.6 s on the CPU | extraction | to fix |
| 15 | auto mode used nvCOMP and gpuinflate where the CPU was faster (the ROM: 3.75 s, CPU 2.98 s) | extraction | fixed on a branch: 2.37 s |
| 16 | A bad sector in a large tar.gz leaves every file "unverified" though 99.8% are identical | rescue | by design (finding 2's rule); the summary says how many decoded cleanly |
| 17 | 70,000 tiny files: ripunzip 8.0 s, gpuzip 9.0 s on Windows, where creating files is the cost | extraction, Windows | to study: how ripunzip spreads its writes |
| 18 | GNU gzip's and pigz's block sizes read as damage: rescue flagged 181 blocks in a clean python.org tarball and lost thousands of files on every kind of damage | rescue | fixed on the same branch, checked here: their counts recognized (Python-3.14.0.tgz: 3,000-3,700 more files back per case; ZIPs unchanged) |

The numbers behind each finding are in the scripts' reports (`report/*.json`, `*.md`), which the
commands above write again.
