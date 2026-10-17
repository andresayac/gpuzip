# Changelog

## Unreleased

### Changed
- `auto` starts nvCOMP only when the CPU threads, timed on the archive's first entries, would
  be slower alone than with the GPU, start-up included: on a strong CPU it is as fast as
  `--mode cpu`, on a weak one the GPU still takes the small entries.
- Large ZIP entries go to gpuinflate by its speed on each entry's compression ratio (about
  3.5 GB/s on text, 1 GB/s on streams that hardly compress) and by when a CPU thread would be
  free for them: a ROM ZIP of nine 750 MB parts tests in 2.48 s instead of 2.93 s, and with
  2 CPU threads in 4.29 s instead of 4.81 s.
- After a GPU error from gpuinflate, the entry and every large entry after it go to the CPU
  threads at once, and gpuinflate is not used again in that run. The note says what failed,
  instead of counting it among the GPU results that failed the CRC check.

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
