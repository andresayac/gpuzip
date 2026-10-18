# Changelog

## Unreleased

### Rescue
- Small files and repetitive data packed into one Deflate block are rescued honestly: zeros (or
  the 0xFF of erased flash) inside a block mark where its damage begins, so what came before them
  is exact and the rest flagged, and a one-block stream whose CRC fails though it decoded in
  step is flagged from them on instead of every byte called exact. On 159 such files, bytes
  wrongly marked exact went from 57 million to 13.
- A rescue's memory no longer grows with its output, only with the damage: a damaged .gz of 2 GB
  peaked at 24.8 GB and now takes 1.2 GB; linux-6.6.tar.gz, damaged eleven ways, 11-13.6 GB and
  now 0.5-0.8 GB; Python-3.14.0.tgz 1.2-1.9 GB and now 0.5-0.9 GB. Past the first 128 MB the
  output is decoded again as it is written, and a .tar.gz is extracted as it comes. What is
  recovered is the same, byte for byte and file for file (66 real-world and 66 generated damaged
  files).
- `--max-output SIZE` stops a rescue whose output would grow past a limit (a decompression bomb):
  by default 100 times the input, at least 4 GB; `0` for none.
- Faster: the search for block headers and the scoring of lost bytes run on every core, and the
  scoring looks again only at bytes whose surroundings changed. A 475 MB ZIP of data that hardly
  compresses went from 15-153 s to 4-39 s per damage, silesia.zip after an FTP transfer from
  136 s to 15 s, enwik9.zip damaged from 9-14 minutes to 1-1.6.
- The search for the CRs an FTP transfer removed tries the second CR's places in parallel and
  stops at 16 GB of decoding per file (1 GB per entry, as before).

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
