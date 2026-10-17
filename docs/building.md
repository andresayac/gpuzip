# Building and installing

Both need [7-Zip](https://www.7-zip.org) for RAR, 7z, tar.* and encrypted entries: `7zz`, `7z`
or `7za` on the PATH, or the usual place on Windows. Use 7-Zip 24 or later (on Linux, 7-Zip's
own `7zz`; on macOS, `brew install sevenzip`): older ones cannot read Zstandard ZIP entries.
The GPU path needs an NVIDIA GPU and driver; without them every entry is decoded on the CPU.

## Rust

Needs [Rust](https://rustup.rs) and a C compiler (the Visual Studio Build Tools on Windows). From
the repository's folder:

```
cargo build --release --manifest-path rust/Cargo.toml
```

The program is `rust/target/release/gpuzip` (`gpuzip.exe` on Windows). It loads three
libraries at run time, each optional: without one, that work goes to the CPU (or, for gzrescue,
`gpuzip rescue` is not available). Each is looked for in the folder its variable names, next to
the program, then on the system's search path (PATH on Windows, LD_LIBRARY_PATH and the linker
cache on Linux):

| Library | Windows | Linux | macOS | Variable | For |
|---|---|---|---|---|---|
| nvCOMP | `nvcomp64_6.dll` | `libnvcomp.so.6` | | `GPUZIP_NVCOMP_DIR` | small ZIP entries on the GPU |
| gpuinflate | `gpuinflate.dll` | `libgpuinflate.so` | | `GPUZIP_INFLATE_DIR` | `.gz`, `.tar.gz` and large ZIP entries on the GPU |
| gzrescue | `gzrescue.dll` | `libgzrescue.so` | `libgzrescue.dylib` | `GPUZIP_RESCUE_DIR` | `gpuzip rescue` |

- nvCOMP is NVIDIA's own library, not in this repository. It comes with
  `pip install nvidia-libnvcomp-cu13` (in `site-packages/nvidia/libnvcomp`, under `bin` on
  Windows and `lib64` on Linux) or NVIDIA's nvCOMP download. gpuzip needs version 6.
- gpuinflate: `gpuinflate\build.cmd release` on Windows, `gpuinflate/build.sh release` on Linux.
  Both need the CUDA toolkit (see [gpuinflate/README.md](../gpuinflate/README.md)).
- gzrescue is CPU only: `gpuinflate\build.cmd rescue` builds it with the Visual Studio C++
  compiler, `gpuinflate/build.sh rescue` with any C++20 compiler (g++, clang++). Without
  `rescue`, both scripts build it first and then gpuinflate.

Running needs no CUDA toolkit: the NVIDIA driver is enough. `gpuzip doctor` shows what was found.

## Python

Needs Python 3.10+ (3.14 for Zstandard entries). From the repository's folder, as a command
available everywhere:

```
uv tool install "./python[gpu]"
```

or into a virtual environment (`pip install "./python[gpu]"`, or `pip install ./python` for
CPU only).

Both install a command called `gpuzip`; if both are on your PATH, the first one found wins.

## Tests

`python tests/test_gpuzip.py` checks the Python version; with `GPUZIP_BIN` pointing at
the Rust build (`rust\target\release\gpuzip.exe`, `rust/target/release/gpuzip`), it checks
that. They pack a folder with 7-Zip, Python and the system's own tool (PowerShell on Windows,
Info-ZIP's zip elsewhere), extract it in every mode and compare every file; without a GPU the
gpu modes are skipped. `cd rust && cargo test --release` runs the unit tests.

## Platforms and CI

| | Windows | Linux | macOS |
|---|---|---|---|
| ZIP, gzip and tar.gz on the CPU; RAR, 7z and the rest through 7-Zip | yes | yes | yes |
| `gpuzip rescue`: damaged .gz, .tar.gz, .zip and Office files (CPU) | yes | yes | yes |
| Small ZIP entries on the GPU (nvCOMP) | NVIDIA | NVIDIA | no |
| One large stream on the GPU (gpuinflate: .gz, .tar.gz, large ZIP entries) | NVIDIA | NVIDIA | no |

macOS has no CUDA, so there everything is decoded on the CPU, and `gpuzip doctor` says so.
GitHub Actions builds and tests every push to main and every pull request on all three
([ci.yml](../.github/workflows/ci.yml)).
Its runners have no GPU: CI covers the CPU paths, the 7-Zip hand-off and rescue, and checks that
gpuinflate compiles for Linux. The GPU paths of the Rust version pass every test in
`tests/test_gpuzip.py` on an RTX 5080 under Windows 11 and under Linux (Ubuntu 24.04 in WSL2,
same card). The Python version runs on the CPU everywhere; its GPU path (CuPy and nvCOMP's
Python package) has been tested on Windows only.

## Release packages

[release.yml](../.github/workflows/release.yml) builds what people download, one archive per
system:

| Package | Holds | Built on |
|---|---|---|
| `gpuzip-<version>-windows-x64.zip` | `gpuzip.exe`, `gzrescue.dll`, `gpuinflate.dll` | Windows Server 2022, Visual Studio 2022, CUDA 13.3 |
| `gpuzip-<version>-linux-x64.tar.gz` | `gpuzip`, `libgzrescue.so`, `libgpuinflate.so` | Ubuntu 22.04 (glibc 2.35), CUDA 13.3 |
| `gpuzip-<version>-macos-universal.tar.gz` | `gpuzip`, `libgzrescue.dylib` (Apple silicon and Intel) | macOS, Xcode's clang |

Each also holds the README, the changelog, the two licenses and `GPU.md`, which says how to add nvCOMP (NVIDIA's
library is not shipped). gpuinflate is compiled for every NVIDIA GPU from Turing (RTX 20) on,
with PTX for newer ones. Before packing, the workflow runs the program from the package folder:
`gpuzip doctor` must load the rescue library from next to it, and a gzip file with a zeroed
sector must be rescued.

To release: set the version in `rust/Cargo.toml` (and `__version__` in `python/gpuzip.py`), add
its section to `CHANGELOG.md`, then push a tag `v<version>` on main. The workflow drafts a
GitHub release with the three packages and their `SHA256SUMS`; read it over and publish it. A
pull request that touches the packaging builds the packages too, without publishing them.
