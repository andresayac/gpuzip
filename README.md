# gpuzip

**Get back what a damaged archive still holds, and extract healthy ones fast: on any CPU, and
faster still on an NVIDIA GPU.**

[![CI](https://github.com/andresayac/gpuzip/actions/workflows/ci.yml/badge.svg)](https://github.com/andresayac/gpuzip/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/andresayac/gpuzip?color=2a78d6)](https://github.com/andresayac/gpuzip/releases)
![Windows · Linux · macOS](https://img.shields.io/badge/runs_on-Windows_·_Linux_·_macOS-2a78d6)
![NVIDIA GPU optional](https://img.shields.io/badge/NVIDIA_GPU-optional-0ca30c)
![License: MIT or Apache-2.0](https://img.shields.io/badge/license-MIT_or_Apache--2.0-52514e)

gzip, tar and 7-Zip stop at the first damaged byte. `gpuzip rescue` keeps going: it decodes a
broken `.gz`, `.tar.gz`, `.zip` or Office document block by block, gives back every file that
survived, and says exactly which bytes are certain and which are not.

<img alt="gzip gives up on a tar.gz with three bad disk sectors; gpuzip rescue gives back 1,690 files intact and marks the rest" src="docs/img/demo-rescue.svg">

- **Rescue** damaged `.gz`, `.tar.gz`, `.zip`, `.docx`, `.xlsx` and `.pptx` files: bad disk
  sectors, interrupted or wrongly resumed downloads, FTP transfers in ASCII mode, bit rot. Every
  file comes out marked intact, inferred, possibly damaged or partial, and the report says what
  could not be checked. Office documents are rebuilt so that Word, Excel and PowerPoint open them.
- **Extract and test** ZIP, `.gz` and `.tar.gz` on every CPU core: a ZIP tests 14-20x faster
  than with 7-Zip and extracts 1.5-8x faster (creating the files is most of that time), every CRC
  checked, paths that would escape the output folder refused. RAR, 7z and the rest go to 7-Zip.
- **On an NVIDIA GPU**, one large `.gz` is decoded in parallel by thousands of GPU threads with
  [gpuinflate](gpuinflate), a decoder of our own: 6.4 GB in 2.2 s, 9.5x 7-Zip and 1.5x
  rapidgzip on 16 threads. The GPU is optional; without one everything runs on the CPU.

## Rescue

Eleven ways files really break, each applied to a log, a tar.gz and a ZIP, then opened with the
usual tools and with `gpuzip rescue`:

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/rescue-dark.svg">
  <img alt="What comes back from a damaged archive: gpuzip rescue against gzip, tar, 7-Zip and Python zipfile, for eleven ways files break" src="docs/img/rescue-light.svg">
</picture>

| Damaged file | gzip, tar, 7-Zip, zipfile (best) | gpuzip rescue |
|---|---|---|
| tar.gz of Python's library, three bad sectors | 87 of 1,828 files | **1,722** |
| the same, a download resumed with 64 KB twice | 544 files | **1,748** |
| the same, sent through FTP in ASCII mode | 0 files | **all 1,828** |
| ZIP of 3,440 files, resumed past a 512 KB gap | 1,729 entries | **3,326** |
| the same, every CR LF turned into LF | 3 entries | **3,429** (233 CRs put back) |
| docx, xlsx, pptx, all eleven failures | 1 of 33 still opens | **32 rebuilt that open clean in Word, Excel, PowerPoint** |

Byte by byte, the same tar.gz with three bad sectors:

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/rescue-map-dark.svg">
  <img alt="One damaged tar.gz byte by byte: gzip gives the first 7.6% right, gpuzip rescue 96.4% exact" src="docs/img/rescue-map-light.svg">
</picture>

And a Word document that 7-Zip opens 2 of 10 parts of, after an FTP transfer in ASCII mode:

<img alt="gpuzip rescue puts back the CRs an FTP transfer removed from a Word document and rebuilds it" src="docs/img/demo-office.svg">

How it works, what each status means and every measurement: [docs/rescue.md](docs/rescue.md).

## Speed

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/zip-speed-dark.svg">
  <img alt="Testing ZIP archives: gpuzip on the CPU is 14-20x faster than 7-Zip" src="docs/img/zip-speed-light.svg">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/gzip-speed-dark.svg">
  <img alt="Testing gzip files: gpuzip on the GPU decodes a 6.4 GB stream in 2.24 s" src="docs/img/gzip-speed-light.svg">
</picture>

Where gpuzip does not win, and why: writing many small files is limited by the file system
(and on Windows by Defender), on small `.gz` files rapidgzip still starts faster, and for ZIP
files the CPU beats nvCOMP on the GPU on this machine (so `auto` leaves the GPU off; with fewer
CPU threads it turns it on). All the numbers and how to reproduce them:
[docs/benchmarks.md](docs/benchmarks.md).

## Install

Download your system's package from the
[releases](https://github.com/andresayac/gpuzip/releases). It holds `gpuzip` (`gpuzip.exe` on
Windows), the rescue library and, on Windows and Linux, the GPU decoder: keep them together in
one folder and put that folder on your PATH (gpuzip looks for its libraries next to itself). Then
`gpuzip doctor` says what it found. To update, replace the folder's files with the new package's.

### Windows

In PowerShell, from the folder you downloaded to (no administrator needed):

```powershell
Expand-Archive .\gpuzip-0.2.0-windows-x64.zip "$env:LOCALAPPDATA\Programs"
Rename-Item "$env:LOCALAPPDATA\Programs\gpuzip-0.2.0-windows-x64" gpuzip
[Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "User") + ";$env:LOCALAPPDATA\Programs\gpuzip", "User")
```

Open a new terminal, and `gpuzip doctor`.

### Linux

x86-64, any distribution with glibc 2.35 or newer (built on Ubuntu 22.04):

```sh
mkdir -p ~/.local/opt
tar -xzf gpuzip-0.2.0-linux-x64.tar.gz -C ~/.local/opt
mv ~/.local/opt/gpuzip-0.2.0-linux-x64 ~/.local/opt/gpuzip
echo 'export PATH="$HOME/.local/opt/gpuzip:$PATH"' >> ~/.bashrc
```

Open a new terminal, and `gpuzip doctor`.

### macOS

Apple silicon and Intel, one package (the GPU paths need NVIDIA's CUDA, which macOS lacks):

```sh
mkdir -p ~/.local/opt
tar -xzf gpuzip-0.2.0-macos-universal.tar.gz -C ~/.local/opt
mv ~/.local/opt/gpuzip-0.2.0-macos-universal ~/.local/opt/gpuzip
xattr -dr com.apple.quarantine ~/.local/opt/gpuzip
echo 'export PATH="$HOME/.local/opt/gpuzip:$PATH"' >> ~/.zshrc
```

`xattr` lifts the mark a browser puts on what it downloads, without which macOS refuses to run a
program that is not signed. Open a new terminal, and `gpuzip doctor`.

### 7-Zip (optional)

[7-Zip](https://www.7-zip.org) 24 or later handles RAR, 7z, tar.xz and encrypted entries, which
gpuzip hands to it: on Windows its installer (gpuzip finds it in its usual folder), on Linux
7-Zip's own `7zz` on the PATH, on macOS `brew install sevenzip`.

### nvCOMP (optional, ZIP on an NVIDIA GPU)

On an NVIDIA GPU (Turing, RTX 20, or newer) gpuzip decodes large .gz and .tar.gz files with
gpuinflate, which is in the package and needs only the NVIDIA driver. For ZIP files it can also
use NVIDIA's nvCOMP, which is not in the package (it is NVIDIA's library). Most machines do not
need it: gpuzip gives ZIP entries to the GPU only when that beats the CPU threads, which it does
with 2 to 4 of them and not on an 8-core desktop CPU ([benchmarks](docs/benchmarks.md));
`gpuzip doctor` says which this machine is. To add it, from gpuzip 0.2.1 on:

```
gpuzip doctor --get-nvcomp
```

It downloads NVIDIA's package of nvCOMP 6 from PyPI (20 to 30 MB), checks it against the SHA-256
PyPI gives, and puts the library and NVIDIA's license in your own folder (`%LOCALAPPDATA%\gpuzip`
on Windows, `~/.local/share/gpuzip` on Linux), where gpuzip looks for it: no administrator, and
an update of gpuzip leaves it there. With gpuzip 0.2.0, take it from pip and copy its library
next to gpuzip:

```powershell
python -m pip install "nvidia-libnvcomp-cu13<7"
$nv = python -c "import nvidia.libnvcomp, os; print(os.path.dirname(nvidia.libnvcomp.__file__))"
Copy-Item (Get-ChildItem $nv -Recurse -Filter nvcomp64_6.dll)[0].FullName "$env:LOCALAPPDATA\Programs\gpuzip"
```

```sh
python3 -m venv ~/.local/share/nvcomp
~/.local/share/nvcomp/bin/pip install "nvidia-libnvcomp-cu13<7"
cp "$(find ~/.local/share/nvcomp -name 'libnvcomp.so.6' | head -1)" ~/.local/opt/gpuzip/
```

(PowerShell on Windows; on Linux in a venv of its own, which recent distributions require for
pip: on Debian and Ubuntu `sudo apt install python3-venv` first.) Instead of copying it,
`GPUZIP_NVCOMP_DIR` can name the folder that holds it. `gpuzip doctor` then says
`nvCOMP 6... test decode ok`.

### From source

With [Rust](https://rustup.rs) and a C++ compiler:

```
cargo build --release
sh gpuinflate/build.sh rescue                  # Linux, macOS (gpuinflate\build.cmd rescue on Windows)
cp gpuinflate/libgzrescue.* target/release/
```

Everything else, and the GPU decoder: [docs/building.md](docs/building.md).

## Usage

```
gpuzip rescue broken.tar.gz       recover what a damaged .gz, .tar.gz, .zip or Office file still holds
gpuzip x photos.zip               extract into ./photos
gpuzip x *.zip -o out             extract each archive into out/<name>
gpuzip x logs.tar.gz              extract the tar inside into ./logs
gpuzip x dump.sql.gz              decompress to ./dump.sql, like gunzip
gpuzip x backup.rar               RAR, 7z and other tar.* go to 7-Zip
gpuzip t backup.zip               check every file's CRC, write nothing
gpuzip l backup.zip               list entries and where each is decoded
gpuzip doctor                     check the GPU, nvCOMP and 7-Zip
```

| Option | What it does |
|---|---|
| `-o DIR` | output folder (default: the archive's name, in the current folder) |
| `--mode auto\|gpu\|cpu` | `auto` (default): GPU and CPU share the work, each where it is faster · `gpu`: all Deflate on the GPU · `cpu`: no GPU |
| `--password PW` | password for encrypted entries (passed to 7-Zip) |
| `--codepage CP` | code page of ZIP names not flagged UTF-8: `932` Japanese, `936` Chinese, `949` Korean, `866` Russian, `850`... (default: the system's OEM page on Windows, as Explorer reads them; 437 elsewhere) |
| `-v`, `-q` | more detail (every damaged file and block, GPU timings) · print only failures |
| `--json` | one JSON report per archive, for scripts |
| `--fill N` | `rescue`: the byte written where the data cannot be known (default 0) |
| `--max-output SIZE` | `rescue`: stop when a file's output would be larger (a decompression bomb); `0` for no limit (default: 100 times the input, at least 4G) |

`x`, `t` and `l` also answer to `extract`, `test` and `list`. Exit codes: 0 ok · 1 missing file
or setup problem · 2 something failed, was damaged or lost.

## Platforms

| | Windows | Linux | macOS |
|---|---|---|---|
| Rescue, extraction and testing on the CPU; 7-Zip for the rest | yes | yes | yes |
| Small ZIP entries on the GPU (nvCOMP) | NVIDIA | NVIDIA | no |
| One large stream on the GPU (gpuinflate) | NVIDIA | NVIDIA | no |

[CI](.github/workflows/ci.yml) builds and tests every pull request on all three. The GPU paths
pass the same tests on an RTX 5080 under Windows 11 and under Linux (Ubuntu 24.04 in WSL2).

## How it works

Every entry in a ZIP file is compressed on its own, so they are decoded in parallel: on every
CPU core with libdeflate, or, in batches of thousands, on the GPU with NVIDIA's nvCOMP. One large
Deflate stream (a `.gz`, a `.tar.gz`) cannot be split that way; gpuinflate finds the block
starts of the stream, decodes from all of them at once on the GPU and stitches the pieces
([research/gpu-inflate](research/gpu-inflate) tells how).

`gpuzip rescue` uses the same block finder on the CPU: it decodes a damaged stream block by
block, knows a block went through damage when its decode does not land exactly on the next
block, restarts after it, and marks every byte that copied from damaged data as unknown
([research/recovery](research/recovery) has the method and its measurements).

More in [docs/how-it-works.md](docs/how-it-works.md). The repository also holds research on a
GPU compressor that writes standard zstd ([research/gpu-zstd](research/gpu-zstd)).

## Contributing

Issues and pull requests are welcome; [CONTRIBUTING.md](CONTRIBUTING.md) says how to build, test
and measure. A bug report is most useful with the output of `gpuzip doctor` and, for a rescue,
`--json`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in gpuzip by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or conditions.
