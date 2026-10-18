# gpuzip rescue: getting back what a damaged archive still holds

`gpuzip rescue` reads a damaged `.gz`, `.tar.gz`, `.zip`, or an Office document (`.docx`,
`.xlsx`, `.pptx` are ZIPs): an interrupted download, a bad sector, a flipped bit.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="img/rescue-dark.svg">
  <img alt="What comes back from a damaged archive: gpuzip rescue against gzip, tar, 7-Zip and Python zipfile, for eleven ways files break" src="img/rescue-light.svg">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="img/rescue-map-dark.svg">
  <img alt="One damaged tar.gz byte by byte: gzip gives the first 7.6% right, gpuzip rescue 96.4% exact" src="img/rescue-map-light.svg">
</picture>

## gzip and tar.gz

gzip stops at the first damaged byte and loses everything after it; worse, Deflate
often decodes damage as plausible garbage that only the trailer's CRC reveals, with no
location. gzrescue (`gpuinflate/rescue.cpp`, the method and measurements in
[research/recovery](../research/recovery)) finds every block header in the file with the finder
from gpuinflate, decodes block by block and knows a block went through damage when its decode
does not land exactly on the next header, or holds a number of symbols other than the
encoder's regular one. The damaged block is kept but flagged, decoding restarts at the next
header, bytes that copied from a damaged block are marked unknown (written as `--fill`), and
the block's true length comes from the trailer so the offsets after it stay right. A file that
went through an FTP transfer in ASCII mode (every LF made CR LF) is put back first, for every
format, and in a ZIP whose transfer went the other way (every CR LF made LF) the lost CRs are put
back entry by entry, where each entry's CRC confirms them; the zeros a crash or an unfinished download leaves at the end are not taken for data;
and a block that decoded through zeros to the end of the file is caught by its symbol count.
Zeros (or the 0xFF of erased flash) inside a damaged block mark where its damage begins: the
block is split there, exact before them, flagged after; a one-block stream whose CRC fails
though it decoded in step is flagged from them on. That is what saves a small file, or
repetitive data, that Deflate packed into one block, with no next header to land on. For
a `.tar.gz` the files are then extracted from the recovered stream. Tar headers copy their
repeated fields from the header before them, so after damage their unknown bytes are made whole
from the previous header, with the checksum as the judge; where a header is beyond repair, the
extractor finds the next readable one byte by byte and reports the files between as lost.
Every file is classified: intact (every byte exact), with inferred bytes (below), possibly
damaged (inside a flagged block, behind a repaired header, or followed by a header out of place),
partial (unknown bytes or cut short). When the checksum fails and no block can be blamed (damage
inside raw stored data, such as an already compressed file, or a flipped bit that kept the
decoder in step), no file is called intact: they are all "unverified", because any of them may
be the one. On a 128 MB tar of source code with a 4 KB stretch zeroed,
7,398 of 7,411 files come back intact (and identical to the originals, checked), 3 possibly
damaged, 2 partial, 8 lost, where gzip gives the first 42% of the archive and nothing else.

## ZIP

A ZIP names every entry twice, in a local header before its data and in the central
directory at the end, and gives each its own CRC. gpuzip reads the directory if it can and scans
the whole file for local headers (skipping those of ZIPs stored inside entries, such as wheels),
so entries the directory does not vouch for, or that an interrupted download cut off from it,
are found too; every entry is recovered, stored ones by copying, Deflate ones through gzrescue,
and its own CRC says whether it is intact. A destroyed local header is worked around with the
directory's metadata and the CRC. On a 17 MB ZIP of 3,447 Python source files: with the
directory zeroed, 3,390 entries intact and 1 cut short; with a 4 KB stretch zeroed inside,
3,436 intact and the 4 entries under the zeros partial; with a header destroyed, every entry
intact.

## Office documents

After the ZIP is recovered, each damaged XML part is made well-formed
again (garbage, broken tags, duplicate attributes, undeclared names and stray declarations
dropped, open elements closed), the text is extracted to `recovered-text.txt`, and a document is
rebuilt from the recovered parts (`recovered.docx`, `.xlsx` or `.pptx`) that an Office
application can open: parts with nothing left are left out with every reference to them, a
damaged worksheet keeps only its valid cells (a well-formed sheet can still hold a cell at
"1A168" or a number "172.t>", which no spreadsheet opens), and a workbook whose main part was
lost gets one listing the sheets found. Word refuses a story that is well formed but not
WordprocessingML (a paragraph inside a run's text, text loose in the body), so a damaged one is
rebuilt from the elements that keep to Word's structure, the text of the others kept as plain
paragraphs; a presentation that lost its slide master, layout, theme or the presentation part
itself gets them made, plainly. What was inside the damaged stretch of the XML is gone; what
came before and after it is kept, and the report lists every change.

## Inferred bytes

After a damaged block the bytes that copied from it are unknown, but every
copy of one lost byte holds the same value, and it is often copied to many places. gzrescue
infers such values from the known bytes around every place they appear and the file's own
statistics, and takes one only when the evidence is overwhelming (99.9% right in the
measurements; the misses were true ambiguities, `operator.lt` for `operator.le`). They are
reported as inferred, never as exact; `--no-infer` in gzrescue leaves them unknown.

## Tested on damage as it happens

[research/recovery](../research/recovery) breaks six files (a
log, a tar.gz, a ZIP, a docx, an xlsx, a pptx) in eleven ways modelled on real failures: an FTP
transfer in ASCII mode either way, bad sectors read with ddrescue, a lost flash erase block,
cross-linked clusters, a stalled torrent, a download cut, resumed twice over or past a gap, a
crash during a copy, bit rot. Some of the 66 results, against what people reach for:

| Damaged file | gzip, tar, 7-Zip, zipfile (best) | gpuzip rescue |
|---|---|---|
| tar.gz of Python's library, three bad sectors | 87 of 1,828 files | **1,722** |
| the same, a download resumed with 64 KB twice | 544 files | **1,748** |
| the same, sent through FTP in ASCII mode | 0 files | **all 1,828** |
| a 32 MB log.gz, three bad sectors | 6.9% of the log | **44.1%** (the rest marked unknown) |
| ZIP of 3,440 files, resumed past a 512 KB gap | 1,729 entries | **3,326** |
| the same, every CR LF turned into LF | 3 entries | **3,429** (233 CRs put back) |
| docx, xlsx, pptx, all eleven failures | 1 of 33 still opens | **32 rebuilt that open clean in Word, Excel, PowerPoint** (the 33rd lost its document whole) |

Silent errors (bytes or files called exact that are not) appear only under bit rot, where a
flipped bit that kept the decoder in step changed a letter while other damage made the checksum
uncheckable, and in one file of the CR-stripped tar; the report says when the checksum could
not be checked. Every rebuilt document was opened in Word, Excel and PowerPoint (through COM,
read only), and none needed their repair. The cases also taught gpuzip most of the handling
described above.

Exit code 2 says something was damaged or lost.

The charts come from `bench/results-rescue.json` (`research/recovery/evaluate.py`) and `bench/results-rescue-map.json` (`bench/rescue_map.py`); `python bench/charts.py` redraws them.
