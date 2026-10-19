# Changelog

## 0.2.0 (2026-10-19)

Extraction that hostile or odd archives cannot fool: entries whose bytes overlap are refused,
names Windows takes for devices are kept, and nothing half written is left under a file's name.
Files come out as they went in: exact times, Unix permissions, read-only, a tar's links on
Windows, names in the code page they were written in. Rescue reads bzip2, LZMA, Zstandard and
Deflate64 entries too (bzip2 block by block) and a tar's links, in memory that grows with the
damage, not the output. The Python version is gone, and the crate is at the repository's root.

### Rescue
- ZIP entries compressed with bzip2, LZMA, Zstandard or Deflate64 are rescued: each decoded (by
  decoders written in Rust: bzip2's libbz2-rs-sys, lzma-rs, ruzstd, deflate64) and called intact
  when its CRC checks. Before, every such entry was "not readable here", even in a sound archive
  (7-Zip's LZMA ZIP: 26 of 428 files back). A damaged bzip2 entry is rescued block by block, as
  bzip2recover does: every block that comes to its own CRC is kept, in place, and only the
  blocks under the damage are lost (a 5 MB entry with 4 KB zeroed: 83% back, where 35% came
  before the break). A damaged LZMA, Zstandard or Deflate64 entry gives what came before its
  break, unchecked: their blocks depend on the ones before. Building needs Rust 1.87.
- Small files and repetitive data packed into one Deflate block are rescued honestly: zeros (or
  the 0xFF of erased flash) inside a block mark where its damage begins, so what came before them
  is exact and the rest flagged, and a one-block stream whose CRC fails though it decoded in
  step is flagged from them on instead of every byte called exact. On 159 such files, bytes
  wrongly marked exact went from 57 million to 13.
- A rescue's memory no longer grows with its output, only with the damage: a damaged .gz of 2 GB
  peaked at 24.8 GB and now takes 1.2 GB; linux-6.6.tar.gz, damaged eleven ways, 11-13.6 GB and
  now 0.5-0.8 GB; Python-3.14.0.tgz 1.2-1.9 GB and now 0.5-0.9 GB; enwik9.zip with 1,835
  damaged blocks 13.4 GB and now 4.3 GB. Past the first 128 MB the output is decoded again as it
  is written, and a .tar.gz is extracted as it comes; where lost bytes were copied to is packed
  in 10-14 bytes a place. What is recovered is the same, byte for byte and file for file (66
  real-world and 66 generated damaged files).
- `--max-output SIZE` stops a rescue whose output would grow past a limit (a decompression bomb):
  by default 100 times the input, at least 4 GB; `0` for none.
- Faster: the search for block headers and the scoring of lost bytes run on every core, and the
  scoring looks again only at bytes whose surroundings changed. A 475 MB ZIP of data that hardly
  compresses went from 15-153 s to 4-39 s per damage, silesia.zip after an FTP transfer from
  136 s to 15 s, enwik9.zip damaged from 9-14 minutes to 1-1.6.
- The search for the CRs an FTP transfer removed tries the second CR's places in parallel and
  stops at 16 GB of decoding per file (1 GB per entry, as before).
- tar.gz files from GNU gzip (python.org, gnu.org) and pigz (kernel.org) are no longer taken for
  damaged: GNU gzip ends some blocks at a multiple of 4,096 symbols and pigz at the end of each
  128 KiB chunk, where zlib always fills them to 16,383; a `.gz` stored inside the tar is told
  apart from the stream around it; and a stream whose CRC checks is never flagged. An undamaged
  Python-3.14.0.tgz had 181 blocks flagged and 1,245 of its 5,045 files called intact; now all.
  Its damaged copies give back 4,000 to 4,900 files instead of about 1,300. Block headers of a
  `.gz` inside that lie in the outer stream's own stored blocks, in their data or their length
  fields, are never taken for the stream's: before, where the blocks fell could make an undamaged
  tar.gz look damaged (seen on CI with this repository's source; 8 of 200 placements with zlib,
  15 with zlib-ng; then 1 of 120 with zlib-ng, a header found in a stored block's length field).
- Damage that can keep the decoder in step (flipped bits, CRs removed by an ASCII transfer)
  leaves no file called intact when the checksum cannot be checked: they are written and listed
  as decoded cleanly but unconfirmed ("unverified" in --json), since the same damage may lie
  where it changed nothing a decoder sees. It is recognized by a block in step with a symbol count
  its encoder never writes (and no zeros to account for it), by a broken block that one flipped
  bit mends, by a CRC that fails though every block stayed in step, or by the transfer itself,
  whose detection no longer misses the few CR LF pairs that CR CR LF leaves. Other data written
  over a cluster can look the same and is treated the same. gzrescue's ABI is now 4 (gr_options
  gains max_output and cr_budget, gr_stats cr_decoded and hidden_damage).
- ZIP entry names not flagged UTF-8, CP437 from Windows tools or UTF-8 from Unix ones, are read
  from local headers as the directory reads them. Before, their non-ASCII bytes became U+FFFD
  there: an entry such as `data/año 2026/` was not matched to its own header and was rescued as
  if its header were destroyed, and one found only by scanning lost its name. Headers and the
  directory are now matched on the name's bytes.
- A ZIP entry whose local header is destroyed is rescued from where its data really starts. Its
  data was looked for at up to six places after where the header was, each a whole rescue, and on
  a tie the first was kept: with an extra field (Info-ZIP writes 28 bytes) that was the wrong one,
  its output cut short and out of place. Now the place where a Deflate stream begins is rescued
  alone; else the one whose data ends where the next header begins comes first; inside a hole of
  zeros or 0xFF one rescue is enough. A 475 MB ZIP with torrent holes: 41 s to 14 s, the same
  bytes. Of 3,318 such entries in nine real ZIPs damaged eleven ways, 4 come back better (one
  from 2,325 bytes right to 284,706 of 566,666, one from 0 to 8,654 of 8,659), none worse.

- A decode that resynchronizes inside a fixed-Huffman block, tried when no block header is left
  after the damage, is believed only when it goes on long enough to trust or ends where the
  stream must (its trailer the file's last bytes, or another member after it). The fixed codes
  take almost any bits: in a gzip of one block, 60 KB of code with 512 bytes zeroed, a decode
  from a wrong place ended in a "final" block 188 bytes on, 5,474 bytes before the trailer, and
  21 wrong bytes were called exact.
- A ZIP entry whose local header spells its name otherwise than the directory (in CP437 with
  Info-ZIP's Unicode Path field, the directory in UTF-8) is rescued from its own header. Before,
  every such entry of a sound archive was rescued as if its header were destroyed, partial,
  and when bytes were written twice its left-over header came out as a second entry of that
  name, written over the first: in a Python ZIP of 428 files resumed badly, 66 entries were
  called intact while the files on disk under their names were partial copies.
- An entry that does not come out intact is never written over a file of the same name that
  another entry gave back intact.
- A ZIP with something before it (a self-extractor's stub, another file's bytes) that lost its
  directory, cut short, is rescued as a ZIP: one local header whose data ends where the next
  header begins says so. Before, it was rescued as a lone Deflate stream and no file came back
  (1 MB before a Python ZIP of 428 files, cut: 0 files, now 221 intact, as without the prefix).
- A tar.gz's links are rescued: symbolic links as `x` makes them (on Windows without the right
  to make them, hard links to the files they name and copies of folders, with a note), hard
  links as links to the file they name. Before, a rescue wrote none, and said nothing:
  linux-6.6.tar.gz, rescued sound, came back without its 63 links.

### Changed
- The program's crate moved from `rust/` to the repository's root, as a Rust project's usually
  is: `cargo build --release` there, and the program is `target/release/gpuzip`. The folder
  stood beside the Python version's, which is gone.
- `auto` starts nvCOMP only when the CPU threads, timed on the archive's first entries, would
  be slower alone than with the GPU, start-up included: on a strong CPU it is as fast as
  `--mode cpu`, on a weak one the GPU still takes the small entries.
- Large ZIP entries go to gpuinflate by its speed on each entry's compression ratio (about
  3.5 GB/s on text, 1 GB/s on streams that hardly compress) and by when a CPU thread would be
  free for them: a ROM ZIP of nine 750 MB parts tests in 2.48 s instead of 2.93 s, and with
  2 CPU threads in 4.29 s instead of 4.81 s.
- A .tar.gz's files of up to 256 KB are written on several threads (`--io-threads`), as a ZIP's
  are: creating files was most of the time where there are many, and one thread did it. 30,000
  small files extract in 4.6 s instead of 12.3 on Windows (the same in a ZIP: 4.3 s), when
  Defender leaves them alone; inspecting each, it still sets the pace (16.5 s instead of 18.5,
  as for the ZIP). A path written twice keeps its last copy, and a link waits for the files
  before it, as in a tar.
- After a GPU error from gpuinflate, the entry and every large entry after it go to the CPU
  threads at once, and gpuinflate is not used again in that run. The note says what failed,
  instead of counting it among the GPU results that failed the CRC check.

### Fixed
- On Windows, gpuzip's libraries (gzrescue, gpuinflate, nvCOMP) are never loaded from the
  current folder: one not next to gpuzip.exe was asked for by its bare name, and Windows looks
  in the current folder before PATH, so a DLL left there (beside a downloaded archive) ran
  inside gpuzip. They come from gpuzip's folder, the folder their GPUZIP_*_DIR names, or PATH's
  folders; the NVIDIA driver from System32; and whatever they load in turn from their own
  folder, gpuzip's and System32.
- On Windows without an NVIDIA GPU, `gpuzip doctor` and every `x` or `t` of a .gz or .tar.gz of
  8 MB or more in auto mode (the default) crashed (access violation): gpuinflate.dll, in the
  Windows package since 0.1.0, was loaded, and its CUDA runtime died without the driver. It is
  now loaded only when the NVIDIA driver says there is a GPU it is built for (Turing or newer).
- ZIP names not flagged UTF-8 are read in the code page they were written in: on Windows the
  system's OEM one, as Explorer and 7-Zip read them, and DOS's 437 elsewhere, as Info-ZIP's
  unzip. Before, 437 everywhere: a Spanish Windows' `ADMINISTRACIÓN/` (code page 850) came out
  `ADMINISTRACIαN/`, 437 having no capitals with accents, and names from a Japanese, Chinese
  or Russian Windows came out garbled with no way to ask for better. `--codepage` now names the
  page, as 7-Zip's `-mcp`: a number (932 Japanese, 936 Chinese, 949 Korean, 950 Traditional
  Chinese, 866 Russian, 850, 852 and the other DOS pages, 1250-1258) or a label (`shift_jis`,
  `gbk`, `cp850`), in `x`, `t`, `l` and `rescue`. Names that look written in another page (box
  drawing, Greek or math signs, or three non-ASCII bytes in a row, where Western letters with
  accents come one or two at a time) get a note saying so and naming the option.
- `x` and `t` run in a terminal no longer wait up to 250 ms after their work for the thread
  that draws the progress line to wake: a ZIP of one entry takes 8 ms instead of 259 ms, 300
  small files extract in 90 ms instead of 260. With stderr redirected (scripts, benchmarks)
  nothing changes: that thread is not started.
- On Windows, an entry named `nul` (or a folder of that name in its path), and on Windows 10
  also `CON`, `PRN`, `AUX`, `COM1`... with any extension, was written to the device and lost,
  `nul`'s with no error. It is written with a `_` before the name, as 7-Zip does, and a note
  says so: in ZIP and tar.gz extraction, a `.gz` named `nul.gz`, and rescue. Only the names this
  Windows takes for a device are renamed (Windows 11 keeps `CON.txt` or `aux.c` as they are).
- An extraction stopped part way (Ctrl-C, a kill, a crash) no longer leaves half a file under
  its name: a file takes its name only once it is whole and its CRC checked. On Windows it is
  created under its name marked to be deleted when closed, and unmarked when whole (a rename per
  file would have made 20,000 small files take 60-116% longer; the mark costs nothing that
  shows). On Linux and macOS it is written as `NAME.gpuzip-part` and renamed. A ZIP entry over
  512 MiB, a file of a .tar.gz and a .gz were the ones written as they decode: killed part way,
  they left 184 of 400 MB, 296 of 600 and 344 of 400 under their names. Nor does a wrong CRC
  leave its output any more (`gpuzip rescue` keeps what can be saved).
- `--mode gpu` sent every member of a gzip file to the GPU, about 10 ms each: a bgzip file of
  3,200 members of 64 KB took 28-34 s, against 0.3 s on one CPU thread. Members of less than
  1 MB now go to the CPU, as small ZIP entries do, with a note; a BGZF file says each member's
  size in its header, so not even `auto` starts the GPU for it. 0.27-0.41 s now.
- A tar's symbolic links failed on Windows ("failed to unpack", exit 2: 63 in linux-6.6.tar.gz),
  where only an administrator or Developer Mode may make them. They are now made once what they
  name is written: real links where allowed, else a hard link to the file they name (which
  needs no privilege) or a copy of the folder (within the archive's size, 64 MB at least), and
  a note says so. A link leading out of the output folder, or naming nothing, is not made.
- ZIP entries get their exact time when the ZIP keeps it: NTFS's (100 ns) or Info-ZIP's
  extended timestamp (seconds, UTC), not the DOS time (local time to 2 seconds, wrong by the time
  zone's offset elsewhere). Folders get their times from ZIP and tar entries once their files
  are written, and the read-only attribute of a ZIP made on DOS or Windows is restored; an
  extraction into the same folder replaces such a file. A tar's folders get their modes last
  too, as GNU tar gives them: one of mode 644 could not be entered, and every file in it failed.
- A ZIP made on Unix or macOS keeps each file's permissions in its directory; they are now
  restored on Linux and macOS, the executable bit among them (setuid, setgid and sticky never),
  by `x` and `rescue`. Not from a ZIP made elsewhere, whose bits there mean nothing.
- Files of a tar whose names differ only in case (13 pairs in linux-6.6.tar.gz) are one file on
  Windows and macOS: the last copy was written over the other with nothing said. A note now
  lists them, as ZIP extraction notes duplicate paths; `x` and `rescue`.
- ZIP entries whose bytes overlap (a local header inside another entry's header or data) are
  refused, by `x` and `t`, as Info-ZIP's unzip and Python's zipfile refuse them: that is how
  zip bombs multiply their data. One 100 MB stream named by 20 entries wrote 2 GB from 103 KB,
  and Fifield's bomb, each entry quoting the next one's header, 200 MB from 12 KB; now nothing,
  exit 2, each entry listed as failed. The other entries are extracted. 109 ZIP files from
  this machine (wheels, Office templates and themes, 14,224 entries) still test clean.
  `rescue` gives such a stream back once: a directory entry whose data would start inside the
  bytes an earlier entry's CRC vouched for is not written (both bombs: one file of 100 and of
  10 MB). The same rule stops it writing other entries' data under a name when a directory's
  offsets are off (lib.zip resumed past a 512 KB gap: 110 such files, nothing in them right), and
  local headers found inside another entry's data, damaged or not, are no longer taken for
  entries (an archive stored in one: 7 in lib.zip; .jar files in silesia's mozilla, 3 junk files
  in each damaged copy; another ZIP's bytes written over a cluster: 2), so a ZIP whose every
  entry is intact is called clean. The 66 damaged files of the realistic tests give back the
  same files of theirs as before.

### Removed
- The Python version (`python/`), the first implementation, kept as a reference. It was never
  released (the packages hold the Rust build only), did less (no rescue, no gpuinflate, .gz and
  tar handed to 7-Zip), and every change had to be made twice. It is in 0.1.0 and 0.1.1. The
  tests and `bench/bench.py` now run the Rust build alone; `bench/results.*` hold the Rust rows
  of their last run.

## 0.1.1 (2026-10-17)

### Fixed
- gpuinflate ran past its input buffer on large streams that compress less than about 2.4:1
  (a ROM image, already compressed data): every such `.gz` or ZIP entry failed on the GPU with
  "invalid argument" and was decoded again on the CPU. The CUDA error also lingered and made the
  next stream fail at once. Both fixed; a 246 MB stream at 0.85 now decodes on the GPU in 0.5 s.

## 0.1.0 (2026-10-17)

The first release.

### Rescue
- `gpuzip rescue` recovers damaged `.gz`, `.tar.gz`, `.zip`, `.docx`, `.xlsx` and `.pptx` files:
  decodes block by block past damage, marks every byte exact, inferred, damaged or unknown,
  skips destroyed tar headers to the next readable one, finds ZIP entries a lost directory no
  longer lists, and rebuilds Office documents that Word, Excel and PowerPoint open.
- Undoes FTP transfers in ASCII mode both ways: LF to CR LF for every format, and CR LF to LF in
  ZIP entries, where each entry's CRC confirms the CRs put back.
- Tested on eleven ways files really break; see [docs/rescue.md](docs/rescue.md).

### Extraction
- ZIP, `.gz` and `.tar.gz` decoded on every CPU core with libdeflate, 14-20x faster than 7-Zip
  when testing; small ZIP entries in batches on the GPU with nvCOMP.
- gpuinflate: one large Deflate stream decoded in parallel on an NVIDIA GPU (6.4 GB in 2.2 s).
- RAR, 7z, other tar.* and encrypted entries handed to 7-Zip; zip slip refused; every CRC checked.

### Platforms
- Windows, Linux and macOS; NVIDIA GPU paths on Windows and Linux. CI on all three.
- Libraries found by their names on each system, next to the program or through `GPUZIP_*_DIR`.
- ZIP names that Unix and macOS tools store in UTF-8 without the UTF-8 flag are read as UTF-8.

### Project
- Dual licensed under MIT or Apache-2.0.
- Charts and terminal sessions in the README drawn from the measured results (`bench/charts.py`,
  `bench/terminal.py`); the documentation moved to `docs/`.
- Release packages for Windows x64, Linux x64 and macOS (universal), built and checked by CI.

### Output
- A summary first: a bar of what was recovered, counts by status, the first files of each
  list; `-v` for everything, `--json` for scripts.
- Colors on a terminal only (`NO_COLOR` respected); a closed pipe ends the program quietly.
