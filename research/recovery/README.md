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

## Damage the way it happens (`realistic.py`, `evaluate.py`)

The cases above are damage chosen to stress the method. `realistic.py` breaks files the way they
break in the world instead, eleven ways, on six originals made from data already on this
machine: a 32 MB Windows CBS log (`gzip -6`), a 32 MB tar.gz of Python's standard library, the
same library as a 17 MB ZIP, and a docx (2,000 paragraphs, styles, two images), an xlsx (six
sheets written by openpyxl: text, numbers, formulas) and a pptx (12 slides with images).

| Failure | How it happens | What it does to the file |
|---|---|---|
| ftp_ascii | FTP in ASCII mode, Unix to Windows | every LF becomes CR LF |
| ascii_strip | the same, Windows to Unix | every CR LF becomes LF |
| bad_sectors | read off a failing disk with ddrescue | three aligned 4 KB sectors are zeros |
| flash_ff | a USB stick or SD card loses an erase block | 128 KB, aligned, of 0xFF |
| crosslinked | chkdsk cross-links two clusters | two 4 KB clusters hold another file's data |
| torrent_holes | a download that stalled at 97% | the pieces never received are zeros |
| cut | an interrupted download | the last 17% missing |
| resume_dup | a download resumed from the wrong offset | 64 KB written twice |
| resume_gap | a download resumed past where it stopped | 512 KB missing in the middle |
| crash_tail | a crash while copying a preallocated file | the last 20% are zeros |
| bitrot | failing RAM or storage | eight random bits flipped |

`evaluate.py` runs `gpuzip rescue` on each of the 66 damaged files, and what people reach for:
`gzip -d` (counted up to its first wrong byte: after it, it stops or decodes garbage), Windows
`tar`, 7-Zip, Python's `zipfile`. It counts what each gives back identical to the original, and
gpuzip's silent errors: bytes it marks exact, or files and entries it calls intact, that are
wrong. Inferred bytes (next section) are counted apart. For the Office documents also the share of
the document's text units (paragraphs, cells, slide texts) found in the recovered XML, and
whether the result opens (every XML part parses, and openpyxl loads the workbook; neither Word
nor Excel was run).

**logs.txt.gz**, the share of the 32 MB log given back right:

| Failure | gzip | gpuzip rescue | silent errors | inferred, wrong |
|---|---|---|---|---|
| ftp_ascii | 0.0% | **100.0%** | 0 | 0 |
| ascii_strip | 1.6% | **25.9%** | 0 | 0 |
| bad_sectors | 6.9% | **44.1%** | 0 | 0 |
| flash_ff | 8.6% | **41.9%** | 0 | 0 |
| crosslinked | 73.7% | **78.6%** | 0 | 0 |
| torrent_holes | 64.2% | **75.6%** | 0 | 0 |
| cut | 85.3% | **85.3%** | 0 | 0 |
| resume_dup | 48.8% | **68.0%** | 0 | 0 |
| resume_gap | 54.2% | **65.6%** | 0 | 0 |
| crash_tail | 81.1% | **81.1%** | 0 | 0 |
| bitrot | 7.1% | **63.7%** | 9013201 | 0 |

**code.tar.gz**, files identical to the original (of 1,828):

| Failure | Windows tar | 7-Zip | gpuzip rescue | silent errors | inferred, wrong |
|---|---|---|---|---|---|
| ftp_ascii | 0 | 0 | **1828** | 0 | 0 |
| ascii_strip | 12 | 12 | **106** | 1 | 0 |
| bad_sectors | 87 | 87 | **1722** | 0 | 0 |
| flash_ff | 207 | 207 | **1771** | 0 | 0 |
| crosslinked | 1208 | 1208 | **1753** | 0 | 0 |
| torrent_holes | 1761 | 1761 | **1761** | 0 | 0 |
| cut | 1462 | 1462 | **1462** | 0 | 0 |
| resume_dup | 544 | 544 | **1748** | 0 | 0 |
| resume_gap | 794 | 794 | **1484** | 0 | 0 |
| crash_tail | 1359 | 1359 | **1359** | 0 | 0 |
| bitrot | 794 | 794 | **1240** | 6 | 0 |

**lib.zip**, entries identical (of 3,440):

| Failure | 7-Zip | zipfile | gpuzip rescue | silent errors |
|---|---|---|---|---|
| ftp_ascii | 0 | 0 | **3440** | 0 |
| ascii_strip | 3 | 0 | **3218** | 0 |
| bad_sectors | 3436 | 3436 | **3436** | 0 |
| flash_ff | 3401 | 3400 | **3401** | 0 |
| crosslinked | 3438 | 3438 | **3438** | 0 |
| torrent_holes | 3252 | 3243 | **3252** | 0 |
| cut | 2654 | 0 | **2654** | 0 |
| resume_dup | 1210 | 2240 | **3440** | 0 |
| resume_gap | 1593 | 1729 | **3326** | 0 |
| crash_tail | 2452 | 0 | **2452** | 0 |
| bitrot | 3432 | 3432 | **3432** | 0 |

**report.docx**, parts identical (of 10), and the document's text and whether it opens:

| Failure | 7-Zip | zipfile | gpuzip rescue | silent errors | text: 7-Zip | text: gpuzip | damaged file opens | rebuilt opens |
|---|---|---|---|---|---|---|---|---|
| ftp_ascii | 0 | 0 | **10** | 0 | 0% | **100%** | no | yes |
| ascii_strip | 2 | 2 | **8** | 0 | 38% | **38%** | no | yes |
| bad_sectors | 8 | 8 | **8** | 0 | 4% | **4%** | no | yes |
| flash_ff | 7 | 7 | **7** | 0 | 0% | **0%** | no | yes |
| crosslinked | 9 | 9 | **9** | 0 | 62% | **62%** | no | yes |
| torrent_holes | 4 | 4 | **4** | 0 | 83% | **83%** | no | yes |
| cut | 2 | 0 | **2** | 0 | 99% | **99%** | no | yes |
| resume_dup | 2 | 7 | **9** | 0 | 68% | **68%** | no | yes |
| resume_gap | 2 | 7 | **9** | 0 | 65% | **65%** | no | yes |
| crash_tail | 2 | 0 | **2** | 0 | 96% | **95%** | no | yes |
| bitrot | 8 | 8 | **8** | 0 | 63% | **63%** | no | yes |

**budget.xlsx** (of 14 parts):

| Failure | 7-Zip | zipfile | gpuzip rescue | silent errors | text: 7-Zip | text: gpuzip | damaged file opens | rebuilt opens |
|---|---|---|---|---|---|---|---|---|
| ftp_ascii | 1 | 0 | **14** | 0 | 0% | **100%** | no | yes |
| ascii_strip | 4 | 6 | **11** | 0 | 17% | **67%** | no | yes |
| bad_sectors | 11 | 11 | **11** | 0 | 67% | **67%** | no | yes |
| flash_ff | 10 | 10 | **10** | 0 | 83% | **83%** | no | yes |
| crosslinked | 11 | 11 | **11** | 0 | 67% | **67%** | no | yes |
| torrent_holes | 12 | 12 | **12** | 0 | 71% | **71%** | no | yes |
| cut | 8 | 0 | **8** | 0 | 84% | **84%** | no | yes |
| resume_dup | 6 | 8 | **14** | 0 | 58% | **100%** | no | yes |
| resume_gap | 6 | 6 | **12** | 0 | 55% | **72%** | no | yes |
| crash_tail | 7 | 0 | **7** | 0 | 82% | **82%** | no | yes |
| bitrot | 10 | 10 | **10** | 0 | 63% | **63%** | no | yes |

**slides.pptx** (of 22 parts):

| Failure | 7-Zip | zipfile | gpuzip rescue | silent errors | text: 7-Zip | text: gpuzip | damaged file opens | rebuilt opens |
|---|---|---|---|---|---|---|---|---|
| ftp_ascii | 0 | 0 | **22** | 0 | 0% | **100%** | no | yes |
| ascii_strip | 22 | 22 | **22** | 0 | 100% | **100%** | yes | yes |
| bad_sectors | 16 | 16 | **16** | 0 | 58% | **58%** | no | yes |
| flash_ff | 0 | 0 | **0** | 0 | 0% | **0%** | no | no |
| crosslinked | 16 | 16 | **16** | 0 | 58% | **58%** | no | yes |
| torrent_holes | 22 | 22 | **22** | 0 | 100% | **100%** | yes | yes |
| cut | 18 | 0 | **18** | 0 | 87% | **85%** | no | yes |
| resume_dup | 13 | 10 | **22** | 0 | 58% | **100%** | no | yes |
| resume_gap | 13 | 6 | **19** | 0 | 58% | **83%** | no | yes |
| crash_tail | 18 | 0 | **18** | 0 | 83% | **83%** | no | yes |
| bitrot | 16 | 16 | **17** | 0 | 76% | **84%** | no | yes |

Another tool gives back more than gpuzip in ten of the 66 cases, by under 0.1 points in nine of
them and by 1.8 points of text in a cut pptx: wherever a stream ends early, its last symbol may
have been read from bits past the end, so gpuzip does not trust what that symbol wrote (up to
258 bytes); the others keep it, and here it happened to be right. The flash-damaged pptx is
16 KB, and the erase block covered all of it. Silent errors: the bitrot cases and one file of the
CR-stripped tar, below; inferred bytes that were wrong: none.

What the cases taught, each a fix now in the code:

- **An ASCII-mode transfer** (every LF to CR LF) defeats every other tool completely. It is
  undone exactly when at least 98% of the file's LFs follow a CR (in compressed data about one in
  256 does): everything comes back, in all six formats. The reverse transfer removed bytes that
  nothing marks; the blocks with none in them decode, the rest are flagged. Putting the CRs
  back by trying them where a decode breaks is not done.
- **A crash leaves zeros where the data should be.** gzip decodes them as data, and so did the
  first version (6 million bytes marked exact that were not). Sixty-four or more zero bytes at
  the end are not gzip's: the input now ends where they begin. gzip's own output is right 131
  bytes further in this case; its garbage after that agrees with the log here and there by
  chance (0.9% of it, which a byte-by-byte comparison would have counted for gzip).
- **A truncated file's last symbol** may be read from bits past its end: the last 258 bytes
  (what one symbol can write) are no longer trusted (94 wrong bytes before).
- **A torrent hole that reaches the end.** A block decoding through 16 KB of zeros writes
  plausible repeated text ("0000...") and runs off the end of the file: it looked like a
  truncated file whose bytes were exact (110,194 wrong bytes). Its symbol count gives it away:
  40,326 in one block, where zlib never puts more than 16,383. That block is now split off and
  flagged; the block before it is exact, to the byte.
- **A lost byte that the decoder survives.** A CR removed inside a block, after which the
  Huffman codes fell back in step with the very same symbol count: the block lands on the next
  header and nothing in the stream shows the damage. The tar does: the next header is not where
  the file's size says it must be. That file is now flagged instead of called intact.
- **Spreadsheets need more than well-formed XML.** After damage a worksheet holds cells at
  "1A168", numbers like "172.t>", a type "=", shared strings that do not exist; openpyxl
  refused the rebuilt workbook in ten of the eleven cases (Excel would at best offer to repair
  it). A damaged sheet
  is now rebuilt from its valid cells only (reference in range and in order, known type, value
  that parses, string and format indices that exist; formulas kept when balanced), a part with
  nothing left is left out with its references, and a workbook cut before its main part (Excel
  and openpyxl write `workbook.xml` near the end) gets one listing the sheets found. All eleven
  now open, with what was dropped listed in the report.
- **A ZIP resumed at the wrong offset** shifts every entry after the splice: entries are matched
  to the scanned local headers by name, CRC and nearest offset, not by the directory's offset.
  When bytes before the directory were lost, its own offset points past the end of the file:
  it is read where it ends, at the end record, as Python's zipfile does. And a compressed
  entry's size no longer hides the local headers inside its declared extent (only a stored entry
  can hold another ZIP's headers): after a gap, the size of the entry before it reaches over the
  entries that follow. The resumed-past-a-gap docx went from 4 parts back to 9.
- **Undoing an ASCII transfer needs a guard**: a sound ZIP of Windows text stored as is has a CR
  before every LF too. A ZIP whose directory ends exactly where its end record says has had no
  bytes inserted, and is left alone (`tests/test_gpuzip.py` checks both ways).
- **Speed**: every entry went through the block-by-block rescue, 25 s for `lib.zip`. An entry
  that inflates to its size and CRC is now taken as it is: 2 s.

The limits that remain:

- **A flipped bit that keeps the decoder in step** and changes one literal into another of the
  same code length (`hasattr` became `haaattr`) is invisible in the stream. When it is the only
  damage, the trailer's CRC fails and no file is called intact. When other damage makes the CRC
  uncheckable, it passes: six files in the bitrot tar, and 9 MB of the bitrot log, where a flip
  in a match length moved the rest of its block by a byte and later blocks copied the moved
  bytes on. The report now says so whenever the CRC cannot be checked: exact then means exact
  as far as decoding can tell.
- **A substitution of the same length** inside a file (one in the CR-stripped tar) leaves the
  tar's headers in place: nothing to see without a checksum per file.
- **LZ77 taint** on repetitive data: with bad sectors the log keeps 44% where gzip gives 7%, and
  the rest is marked unknown, not guessed.

## Inferring the lost window

After a damaged block the bytes that copy from the lost window are unknown, but not free: every
copy of one window byte holds the same value, and a byte is often copied to many places. The
decoder gives each lost window position a variable and carries it through copies (a copy of a
copy is the same variable). gzrescue then infers values from the file's own statistics: a model
of contexts of 1 to 5 bytes, blended from the longest seen down as PPM does, with exact keys,
trained on the 1 MB of exact output nearest the damage. Every byte value is scored by how well
it fits between the known bytes before and after each place the variable appears. A value is
taken only when

- it leads the next by 14 nats (about a million to one) summed over those places,
- one place on its own favours it by 6, and no place favours another by 2,
- and only places with at least two known bytes on each side count (places alike on the three
  bytes either side count once).

Values taken become known for the next round, up to 60 rounds. Inferred bytes are their own
class in the mask (3), never exact, and a tar file holding any is reported as "inferred", not
intact.

| Damaged file | bytes inferred | wrong |
|---|---|---|
| code.tar.gz, bad_sectors | 21,911 | 0 |
| code.tar.gz, flash_ff | 1,830 | 0 |
| code.tar.gz, torrent_holes | 4,057 | 66 |
| logs.txt.gz, bad_sectors | 51,145 | 0 |
| logs.txt.gz, flash_ff | 23 | 0 |

That is 78,966 bytes, 99.92% right (`infer_sweep.ps1`). The 66 wrong ones are two decisions,
each copied many times, and both are true ambiguities: `operator.lt` read as `operator.le`,
which is just as valid. Each rule came from a failure. Summing many weak one-sided places was
right 86% of the time on the tar: in a certificate bundle's fingerprints (`94:45:?`) sixty places
each said "a digit" a little, and together they said it loudly, sixty times wrong. Keeping as
candidates only the bytes ever seen after the byte before let a value never seen there lose
without a contest (676 wrong bytes). Coverage is small: tens of KB out of the megabytes lost,
because most unknown bytes lie in runs with no known neighbour, out of reach of a byte-by-byte
method; decoding whole runs jointly would be the next step.

## What is not done

- gzrecover for comparison (it needs a download); the baseline here is gzip itself.
- Headers whose varying fields (size, name, time) were lost cannot be repaired from the previous
  header; a few more files per damaged region could be saved by searching the checksum over
  a few unknown bytes.
- Fixed-Huffman and stored blocks after damage are restarted at the next dynamic header only;
  a probe-based restart inside a fixed block exists for the case where no header follows.
- The whole recovered stream is held in memory for a tar.gz (twice: bytes and mask).
- CR re-insertion for a file that went through a Windows-to-Unix ASCII transfer.
- Trusting the output of a cut stream to the last whole symbol instead of distrusting its last
  258 bytes: the decoder would have to say where its last symbol began (it is shared with the
  GPU kernel, so not changed for this).
- Joint inference of runs of lost window bytes.
- Opening the rebuilt documents in Word, Excel and PowerPoint themselves; the check here is that
  every XML part parses and openpyxl loads the workbook.
- Real damaged files from the world: the damage here is modelled on how files break, but made
  here.

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
python realistic.py <dir>                                       (six originals, eleven real-world failures each: 66 files; needs openpyxl, Pillow)
python evaluate.py <dir> [--only lib.zip] [--failures cut]      (gpuzip against gzip, tar, 7-Zip, zipfile: results.md; needs numpy, openpyxl)
infer_sweep.ps1 [-SoloAgainst "6:2"] [-Margins "14:60"]         (inference accuracy and coverage by setting)
python infer_debug.py <dir> <damaged.gz> <original> [14] [60]   (wrong inference decisions, with their contexts)
python var_check.py <dir> <damaged.gz> <original>               (every byte of one variable must share one true value)
```

One more lesson from the test suite: damage inside a stored block (zlib keeps incompressible
data raw, so random or already compressed files) changes nothing a decoder can see; only the
trailer's CRC fails, with no location. `gpuzip rescue` then calls no file intact: all are
"unverified", because any of them may be the one.
