# Changelog

## Unreleased

### Rescue
- Small files and repetitive data packed into one Deflate block are rescued honestly: zeros (or
  the 0xFF of erased flash) inside a block mark where its damage begins, so what came before them
  is exact and the rest flagged, and a stream whose CRC fails with every block decoded in step
  gets the block holding them flagged instead of every byte called exact. On 159 such files,
  bytes wrongly marked exact went from 57 million to 13.

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
