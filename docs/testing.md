# Testing gpuzip on real archives

`tests/test_gpuzip.py` checks gpuzip on archives made for it. This page is about everything
else: archives from the wild and from other tools, hostile ones, damage as it happens, other
file systems, a busy GPU, and the tools people already use. The first campaign ran on
2026-10-17, against v0.1.1 and two branches, on an RTX 5080, a Ryzen 7 9700X, Windows 11 and
Ubuntu 24.04 in WSL2; a second pass on 2026-10-18 checked the combined rescue branch. What it
found, and what was done about it, is at the end.

## The scripts

| Script | What it does |
|---|---|
| `bench/make_realworld.py DIR` | makes the archives other tools and odd situations make: 7-Zip at -mx9, AES-256, ZipCrypto, Deflate64, bzip2, LZMA; Python's stored, Zstandard and streamed (data descriptors) ZIPs; PowerShell, Info-ZIP, a JAR; Zip64 by count (70,000 entries) and by size (4.5 GB); a self-extractor's stub before the archive; a 60 KB comment; names Windows refuses; entries that try to leave the folder; gzip with three members, every header field, bgzip's layout; a tar.gz with long names and links |
| `bench/realworld.py --bin A=PATH --bin B=PATH PATHS...` | every archive, every gpuzip given, every mode: `x` and `t`, every extracted file compared by SHA-256 with 7-Zip's extraction (Python's zipfile, or the source folder, when 7-Zip itself fails) |
| `bench/realworld.py --rescue ...` | each ZIP and tar.gz damaged in the eleven ways of `research/recovery/realistic.py`, then `gpuzip rescue`: files back identical, files called intact, silent errors (called intact, but wrong), and peak memory |
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
  tarballs, three generated ZIPs (up to 70,000 entries), and 13 more generated archives on the
  combined rescue branch: no silent errors except the CR LF case below.

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

## Rescue: v0.1.1 against the combined branch

`bench/realworld.py --rescue` with three builds: v0.1.1, the GNU-counts branch (2e65adc), and the
combined branch (`claude/rescue-combined` at aebe20a: findings 2, 3 and 18, plus fewer recorded
places in a stream that lost its CRs). The run was stopped part way on 2026-10-18. Done:

- **13 generated archives**, 11 failures each, 429 runs: 7-Zip's bzip2, Deflate64, LZMA and -mx9
  ZIPs, an empty ZIP, two 300 MB entries that hardly compress, Info-ZIP's, a JAR, a pax tar.gz with
  links, PowerShell's, Python's with a comment, with a stub before it, and stored. 0 silent errors
  in all three builds, and the same files back identical (27,419) and called intact in every
  case. The combined branch took 305 s in all against v0.1.1's 627 s. On the two 300 MB entries its
  peak memory fell from 4.5 to 1.8 GB (torrent_holes) and from 4.3 to 1.4 GB (bad_sectors), but
  not with their CRs stripped: 4.26 to 4.07 GB (finding 21).
- **20 cases again with a stricter count** (an entry called intact under a name the reference
  lacks must hold one of the archive's files): 0 silent. Those entries are finding 20's.
- **`bench/malicious.py --big`:** 14 of 14. The 8 GB gzip bomb, rescued, stops at the 4.3 GB limit
  with a 354 MB peak and exit 1.
- **Python's and the kernel's tarballs**, 22 cases, checked by the session that made the branch:
  every count the same as 07a63e3's, 0 silent. The kernel with its CRs stripped: 2.98 to 2.07 GB,
  40 to 35 s; Python's: 1.11 to 0.92 GB.

Not done: py_streamed, py_zstd, zero_bytes and zip64_70k among the generated archives; enwik9,
silesia and the GitHub zip; the two tarballs against v0.1.1 with this harness's peak memory. Six
runs on empty.zip were reported as failures because rescue exits 1 when a 22-byte archive is
overwritten ("no Deflate data could be recognised"); the harness now accepts exit 1 for an
archive of no files. One time to measure again: py_prepended.zip after resume_dup, 3.6 s with
v0.1.1, 14.1 s and then 7.7 s with the combined branch.

## What it found

| | Finding | Where | State |
|---|---|---|---|
| 1 | gpuinflate ran past its input buffer on large streams that compress less than about 2.4:1: every such entry failed on the GPU and was redone on the CPU | the Android ROM | fixed in 0.1.1 |
| 2 | A tar.gz whose CR LF pairs became LF: files called intact are wrong (Python's tarball: 114 of 163; the kernel's: 5,732 of 8,092) | rescue | fixed on a branch, checked here: with no checksum to confirm them such files are "unverified", never "intact"; 0 silent errors on every case above |
| 3 | Rescue holds 6-8 times the output in memory (2 GB out: 12-17 GB), so a large damaged file, or a small gzip bomb, exhausts RAM | rescue | fixed on the combined branch (a 2 GB .gz: 24.8 GB to 1.1 GB, 80 s to 37 s), checked here in part: see above |
| 4 | A branch that placed damage inside blocks called 53 wrong kernel files intact | rescue branch | fixed before merging (d0f67aa) |
| 5 | Searching for lost CRs in a binary ZIP takes 2 minutes and finds none (silesia) | rescue | bounded on the combined branch (1 GB of decoding an entry, 16 GB a file, and no search in an entry that lost more than 2 bytes). Measured again: silesia after the transfer is rescued in 40 s, none of it searching (each entry lost 10-193 bytes); the time is the block-by-block rescue of its 640 damaged blocks |
| 6 | An entry named `nul` (or `CON`, `PRN`, `AUX`, `COM1`...) is written to the Windows device and lost, with no error | extraction, Windows | fixed: a name this Windows takes for a device (asked with GetFullPathNameW, which opens nothing) is written with a `_` before it, as 7-Zip does, with a note; in ZIP and tar.gz extraction, a `.gz` named `nul.gz`, and rescue. Windows 11 (26300) takes only NUL so inside a folder, Windows 10 all of them; the test asks Windows the same way, so it holds on either (run on 11 only) |
| 7 | tar symbolic links fail on Windows ("failed to unpack", exit 2: 63 in the kernel); 7-Zip writes empty files instead | extraction, Windows | fixed: made once what they name is written, a real link when Windows allows it, else a hard link to a file or a copy of a folder (within the archive's size, 64 MB at least), with a note; one leading out of the folder or naming nothing is not made. Hard links and copies checked here (no Developer Mode: error 1314), real links on CI (an administrator), where a target with / led nowhere until turned to \; the kernel was not run again |
| 8 | tar entries that differ only in case overwrite each other with no note on Windows and macOS (13 pairs in the kernel); ZIP extraction does note it | extraction | fixed: a note lists them (the last copy kept, which the tar order gives and the writer threads keep), in `x` and `rescue`; `tests/test_gpuzip.py` checks both ways. The kernel was not run again |
| 9 | Overlapping ZIP entries are not detected: 100 KB extract to 2 GB | extraction | fixed: an entry whose bytes (local header to the end of its data) overlap another's is refused by `x` and `t`, the others extracted; one stream named by 20 entries and Fifield's quoting bomb (12 KB to 200 MB) write nothing now, and `malicious.py` requires it. 109 ZIPs from this machine, 14,224 entries, none refused. `gpuzip rescue` wrote both out (2 GB and 200 MB, under its 4 GB least limit); now a directory entry whose data would start inside the bytes an earlier intact one took is not written (one file each). Entries only the scan finds set no such bound (crosslinked: an intact entry of lib.zip inside the pptx), empty entries are not held to it; the realistic tables come out the same |
| 10 | The Unix executable bit is not restored from ZIP entries (it is from tar) | extraction, Linux, macOS | fixed: the permission bits a ZIP made on Unix or macOS keeps (the high half of the external attributes) are restored for regular files by `x` and `rescue`, never setuid, setgid or sticky, not from a ZIP made on DOS or Windows |
| 11 | Names from Japanese, Chinese or Russian Windows come out garbled; no way to name the code page (7-Zip has -mcp) | extraction | to add |
| 12 | An interrupted extraction leaves partial files under their final names | extraction | fixed: a file takes its name once whole (Windows: created under it marked delete-on-close, unmarked when whole, since a rename per file cost 260-320 µs here, +60-116% on 20,000 small files; Linux and macOS: `NAME.gpuzip-part`, renamed). `tests/test_gpuzip.py` kills `x` while it writes a tar.gz's file, a .gz and a ZIP entry over 512 MiB: before, 184 of 400 MB, 344 of 400 and 296 of 600 stayed under their names; now nothing. Rescue still writes under the final names |
| 13 | Info-ZIP's extended time is ignored (the local DOS time is used); folder times and the read-only attribute are not restored | extraction | fixed: NTFS's time, else Info-ZIP's extended one, over the DOS time; folder times from ZIP and tar entries, given after their files; the DOS read-only attribute (and on Windows a Unix mode the owner may not write), with an extraction into the same folder still replacing such files |
| 14 | `--mode gpu` on a gzip of thousands of small members (bgzip) takes 35 s against 0.6 s on the CPU | extraction | fixed: members of less than 1 MB go to the CPU, with a note, and BGZF's header gives each member's size so the GPU is not started for them. 3,200 members of 64 KB: 28.5-34.3 s to 0.27-0.41 s; a member of 200 MB still goes to the GPU |
| 15 | auto mode used nvCOMP and gpuinflate where the CPU was faster (the ROM: 3.75 s, CPU 2.98 s) | extraction | fixed on a branch: 2.37 s |
| 16 | A bad sector in a large tar.gz leaves every file "unverified" though 99.8% are identical | rescue | by design (finding 2's rule); the summary says how many decoded cleanly |
| 17 | 70,000 tiny files: ripunzip 8.0 s, gpuzip 9.0 s on Windows, where creating files is the cost | extraction, Windows | explained, kept by choice: measured again with both programs in one folder, gpuzip 9.50 s, ripunzip 7.63 s (more threads change nothing: 9.3-9.7 s with 16, 32, 64). ripunzip gives no file its time (they keep the hour of the extraction), and nothing keeps a killed one from leaving half files (finding 12): without both, gpuzip takes 7.61 s. Each costs about 25 µs a file (SetFileTime; the delete-on-close mark and its removal) |
| 18 | GNU gzip's and pigz's block sizes read as damage: rescue flagged 181 blocks in a clean python.org tarball and lost thousands of files on every kind of damage | rescue | fixed on the same branch, checked here: their counts recognized (Python-3.14.0.tgz: 3,000-3,700 more files back per case; ZIPs unchanged) |
| 19 | Rescue reads only stored and Deflate entries: LZMA, bzip2, Deflate64 and Zstandard ones are listed as "not readable here", undamaged ones too (7-Zip's LZMA ZIP: 26 of 428 files back from the clean archive) | rescue | first step done: bzip2, LZMA, Zstandard and Deflate64 entries are decoded (pure Rust decoders) and their CRC vouches; sound archives of each come back whole (Python's and 7-Zip's: 30 of 30, 42 of 42), and with 4 KB zeroed the other entries stay intact, none wrong called intact. A damaged bzip2 entry is rescued block by block, each block by its own CRC (5 MB with 4 KB zeroed: 35% to 83%, the 5 good blocks of 6 in place); LZMA, Zstandard and Deflate64 give what came before the break, their blocks depending on those before |
| 20 | Rescue names the entries it finds by scanning with � for every byte that is not ASCII, ignoring the Unicode Path field and CP437 that extraction reads: in 7-Zip's ZIP with its CRs stripped, 66 files decoded intact landed in `data/a�o 2026/`, while the same files under their right names came out partial | rescue | fixed (7998917): a local header's name is decoded as the directory decodes it (the UTF-8 flag, Info-ZIP's Unicode Path, else UTF-8 when valid, else CP437) and matched to its directory entry on the name's bytes; `tests/test_gpuzip.py` checks a CP437 and an unflagged UTF-8 name, the directory read and lost. The 17 cases were not run again |
| 21 | A stream that lost its CRs still costs rescue more memory than any other damage on the combined branch: the kernel's tarball 2.07 GB, two 300 MB ZIP entries 4.07 GB (1.1-1.8 GB for the other failures) | rescue | reduced (1ac6f2c): each place is packed in 10-14 bytes instead of 36 (most bytes around one are the next lost bytes, copied along with it). Measured on its branch: the two 300 MB ZIP entries 4.2 to 2.0 GB, enwik9.zip with bad sectors 7.1 to 4.3 GB; the kernel's tarball about the same (few places). The places are still all recorded |
| 22 | Every `x` and `t` run in a terminal waited up to 250 ms after its work for the progress line's thread to wake from its sleep: a ZIP of one entry 259 ms, 8 ms with stderr redirected. Every harness here redirects stderr, so none saw it (found by an outside review, 2026-10-18) | extraction | fixed: the thread is woken when the work ends; 8 ms, and 300 small files extract in 90 ms instead of 260 |

The numbers behind each finding are in the scripts' reports (`report/*.json`, `*.md`), which the
commands above write again.
