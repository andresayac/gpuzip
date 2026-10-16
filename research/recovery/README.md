# gzrescue: recovering damaged gzip files

A one-day feasibility test, October 2026, of the direction gpuzip turned to after the GPU
compressor: tools that help when a `.gz` or `.tar.gz` is damaged (an interrupted download, a
bad sector, a flipped bit). The pieces it is built from are gpuinflate's: the block-header finder
and the resynchronizing decoder.

## The problem

`gzip -d` stops at the first damaged byte: everything after it is lost. Worse, Deflate often does
not notice damage at all. Huffman codes resynchronize, so after a damaged stretch the decoder
falls back into step and produces plausible output; the error surfaces only in the trailer's
CRC, with no location. The one tool for this, gzrecover, dates from 2000s and its author warns it
"can be fooled".

## What gzrescue does

1. **Finds every block header** in the file, wherever it is: the cheap test on every bit
   position and zlib's header checks from gpuinflate's finder, each candidate confirmed by
   decoding two blocks from it. Headers of Deflate streams embedded in the data (a `.gz` inside a
   tar) are found too and told apart later: their chain of blocks ends on its own instead of
   joining the stream's.
2. **Decodes block by block, landing.** A decode that starts at one header and lands exactly on
   the next is in step. One that hits invalid data, runs off the end, reads a "final" block
   where data goes on, or lands on a later header having passed over the next one went through
   damage: that block is kept but marked suspect, and decoding restarts at the next header. Only
   the damaged block is in doubt.
3. **Catches damage that stays in step.** zlib puts the same number of symbols in every block
   (16,383 by default). A block that landed correctly but holds a different count was damaged
   in a way the decoder could not see; a second pass treats it as suspect. (A bit flip that only
   changes one literal into another keeps the count and the alignment: undetectable, and gzip
   cannot see it either.)
4. **Marks what cannot be known.** After a damaged block the 32 KB window is untrusted: bytes
   that copy from it, and whatever copies from those, are marked unknown and written as a fill
   byte. On ordinary data the window refreshes within a few KB; on very repetitive data the
   taint can reach to the end of the file (below).
5. **Keeps the offsets right.** The damaged block's true length is taken from the gzip trailer
   when there is one such block (spread by compressed size when there are several), so the data
   after it sits where it belongs and a tar stays extractable.

Output: the recovered data (`--fill` for unknown bytes), a JSON report of every piece and what
happened to it, and `--mask`: one byte per output byte, exact / suspect / unknown.

## Measured (`damage.py`, `measure.py`)

Two 128 MB originals compressed with zlib level 6: a tar of source code (7,411 files, from the
corpus of `research/gpu-zstd`) and a Windows CBS log. Six kinds of damage each. "gzip gives" is
what a standard decoder produces before failing, as a share of the original; "exact" are bytes
gzrescue marks good that match the original; "uncertain" are bytes it marks unknown; "silent
errors" are bytes it marks good that are wrong, the number that matters most; "warned" are
wrong bytes inside a block it flagged. Alignment is checked against the original, so a shifted
piece is not counted as wrong.

**code.tar** (ordinary data):

| Damage | gzip gives | exact | uncertain | lost | silent errors | warned wrong | files intact (of 7,411) |
|---|---|---|---|---|---|---|---|
| 4 KB zeroed at 50% | 42.4% | **99.63%** | 0.19% | 0.19% | 0 | 245 KB | 7,267 |
| one bit flipped at 30% | 32.1% | **99.82%** | 0.14% | 0 | 165 B | 58 KB | 7,255 |
| 1 KB random at 70% | 61.3% | **99.80%** | 0.14% | 0.02% | 0 | 77 KB | 7,231 |
| cut at 60% | 50.1% | 50.10% | 0.06% | 49.9% | 1 B | 242 KB | 3,489 |
| 512 B zeroed at 25% and a flip at 75% | 29.3% | **98.38%** | 1.01% | 0.27% | 488 KB | 329 KB | 7,052 |
| first 16 bytes zeroed | 0% | **99.82%** | 0.14% | 0 | 0 | 58 KB | 7,276 |

**logs.txt** (very repetitive: ratio 25):

| Damage | gzip gives | exact | uncertain | silent errors | warned wrong |
|---|---|---|---|---|---|
| 4 KB zeroed at 50% | 49.7% | 66.8% | 32.9% | 0 | 390 KB |
| one bit flipped at 30% | 29.1% | 100% | 0 | 1 B | 0 |
| 1 KB random at 70% | 72.7% | 82.2% | 17.5% | 0 | 321 KB |
| cut at 60% | 60.3% | 60.3% | 0 | 0 | 0 |
| 512 B zeroed at 25% and a flip at 75% | 24.8% | 33.8% | 51.1% | 20 MB | 234 KB |
| first 16 bytes zeroed | 0% | 100% | 0 | 0 | 0 |

What it says:

- On ordinary data the damage costs the damaged block and little else: 98-99.8% comes back
  exact and 95-98% of the tar's files are intact, against 30-60% from gzip. The damaged
  block's first part is right too (it is kept, flagged).
- The log shows the limit of LZ77: its lines are copies of earlier lines, so a block that copied
  from garbage passes the garbage on, line after line, to the end. Those bytes are honestly
  marked unknown (a third to a half of the file), not silently wrong; an uncut text is still
  there in the literals. No tool can do better without the lost window.
- The remaining silent errors come from damage that keeps both the block alignment and the
  symbol count (a flipped bit that changes a length or distance, or a literal): the trailer's
  CRC and length then say that something is wrong, not where. The two-damage cases also stress
  the length estimate: with several suspect blocks their true lengths are apportioned, and the
  evaluation may misalign a piece.
- Truncation is what it is: everything before the cut, exactly.

Speed: 5-10 s for a 26 MB `.gz` on one CPU thread; the header scan is the GPU prefilter's job
for large files.

## In gpuzip

The method lives in `gpuinflate/rescue.cpp` (C API in `rescue.h`, built as `gzrescue.dll`, CPU
only); `gzrescue.cpp` here is a command line over it, used by `measure.py`. `gpuzip rescue`
(Rust, `rust/src/rescue.rs`) adds the tar side: the files are extracted from the recovered
stream and classified as intact, possibly damaged or partial. Two things make that work on a
damaged archive:

- Tar headers copy their repeated fields (magic, mode, owner) from the header before them, so
  after damage those bytes are unknown in every later header, and a header with unknown bytes
  does not verify. The extractor fills the unknown bytes of a header with the previous
  header's bytes at the same places, the very bytes the encoder had copied, and the checksum
  says whether the repair is right.
- Where a header is beyond repair, the next readable one is searched for byte by byte (a
  damaged block whose length was only estimated shifts everything after it by an arbitrary
  amount), and the chain of members ending there is walked back, so only the destroyed stretch
  is skipped.

On the zeroed-sector case above, `gpuzip rescue` gives back 7,398 of 7,411 files intact (every
one checked identical to the original by `check_extract.py`), 3 possibly damaged, 2 partial and
8 lost, in about 7 s. (An earlier version gave 7,149: the window of trusted output was one block
deep, so with small blocks references that reached further back were marked unknown.)
`tests/test_gpuzip.py` damages a tar.gz and checks the same properties on the Rust build.

## ZIP and Office documents

A ZIP gives us more to work with than gzip: every entry has its own CRC, and its name and sizes
are stored twice (local header, central directory). `rust/src/ziprescue.rs` reads the
directory when it can, scans the file for local headers (`PK\3\4`; headers of a ZIP stored inside
an entry, such as a wheel, are skipped by tracking the data extents), and recovers each entry:
stored ones by copying, Deflate ones through `gr_rescue_raw`, the library's entry point for a
raw stream with a known size and CRC (they stand in for the gzip trailer). A destroyed local
header is worked around by trying the data at the offsets the directory implies and keeping the
one whose CRC matches. Entries are classified intact (CRC verified), partial (flagged blocks,
unknown bytes, cut short), unverified (in step but CRC mismatch, or no CRC), lost, unsupported.

`make_zip_cases.py` builds a 17 MB ZIP of 3,447 Python source files and a 300-paragraph docx
with six kinds of damage; `check_zip.py` compares the result with the original:

| Damage | intact | partial | lost | note |
|---|---|---|---|---|
| cut at 70% (no directory) | 2,041 | 7 | 1,398 beyond the cut | every entry before the cut, from the local headers |
| directory zeroed (last 3%) | 3,390 | 1 | 49 under the zeros | |
| 4 KB zeroed inside | 3,436 | 4 (the entries under the zeros) | 0 | directory read |
| a local header zeroed | 3,440 | 0 | 0 | the entry found by its CRC |
| docx: 512 zero bytes in `document.xml`'s data | 3 | 1 unverified | | 141 of 300 paragraphs in the text; repaired XML well-formed; rebuilt document opens |
| docx: cut before the directory | 4 | 0 | | all 300 paragraphs; rebuilt document opens |

Every entry called intact was identical to the original in every case. For an Office document
the damaged XML parts are made well-formed again (`repair_xml`: garbage and broken tags dropped,
tags with undeclared namespace prefixes dropped, entities completed, open elements closed), the
text is extracted (`w:t`, `a:t`, `t`), and a document is rebuilt from the recovered parts, stored,
so an Office application can open it. A docx's `document.xml` is usually one Deflate block, so
damage inside it costs the rest of that block's text; the text before it survives.

## What is not done

- gzrecover for comparison (it needs a download); the baseline here is gzip itself.
- Headers whose varying fields (size, name, time) were lost cannot be repaired from the previous
  header; a few more files per damaged region could be saved by searching the checksum over
  a few unknown bytes.
- Fixed-Huffman and stored blocks after damage are restarted at the next dynamic header only;
  a probe-based restart inside a fixed block exists for the case where no header follows.
- The whole recovered stream is held in memory for a tar.gz (twice: bytes and mask).
- ZIP (rebuilding a lost central directory, recovering entries), and Office files (docx, xlsx,
  pptx are ZIPs) on top of this.
- Real damaged files from the world, not only synthetic damage.

## Running it

```
build.cmd                                                       (cl; CPU only: gzrescue.exe over ..\..\gpuinflate\rescue.cpp)
python damage.py <original> <dir>                               (a .gz and six damaged copies)
gzrescue <damaged.gz> <out> [--report r.json] [--mask m.mask] [--fill 0]
python measure.py <dir>                                         (needs numpy)
gpuzip rescue <damaged.tar.gz> -o <dir> --json > r.json         (the Rust build, with gzrescue.dll next to it)
python check_extract.py <original.tar> <dir> r.json             (every file called intact must be identical)
python repro_test.py <work-dir> [0.4]                           (the test suite's damaged tar.gz, with the report and every differing file)
python make_zip_cases.py <dir> [48]                             (a ZIP of Python's library and a docx, six damaged copies)
gpuzip rescue <dir>\lib.nocd.zip -o <out> --json > r.json
python check_zip.py <dir>\lib.zip <out> r.json                  (every entry called intact must be identical; the docx checks)
```

One more lesson from the test suite: damage inside a stored block (zlib keeps incompressible
data raw, so random or already compressed files) changes nothing a decoder can see; only the
trailer's CRC fails, with no location. `gpuzip rescue` then calls no file intact: all are
"unverified", because any of them may be the one.
