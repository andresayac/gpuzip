# Contributing

Thank you for helping. gpuzip reads files that are often damaged and sometimes hostile, and it
tells people which of their bytes they can trust, so two things matter more than anything else
here: it must never call wrong data right, and every claim about speed or recovery must come
from a measurement someone else can repeat.

## Building

[docs/building.md](docs/building.md) has the details. In short, from the repository's folder:

```
cargo build --release
sh gpuinflate/build.sh rescue          # libgzrescue; gpuinflate\build.cmd rescue on Windows
sh gpuinflate/build.sh                 # also gpuinflate, on Linux with the CUDA toolkit
```

Put the libraries next to `target/release/gpuzip` or point `GPUZIP_RESCUE_DIR`,
`GPUZIP_INFLATE_DIR` and `GPUZIP_NVCOMP_DIR` at their folders; `gpuzip doctor` shows what it
finds.

## Testing

```
cargo test --release              # unit tests
python tests/test_gpuzip.py       # the build in target/release (GPUZIP_BIN=... for another)
```

`tests/test_gpuzip.py` packs a folder with 7-Zip, Python and the system's own tool, extracts it
in every mode and compares every byte and timestamp, then damages archives and checks what
`gpuzip rescue` gives back. Without a GPU the gpu modes are skipped; CI runs everything else on
Windows, Linux and macOS for every pull request. After a change to gzrescue
(`gpuinflate/rescue.cpp`), run it with `GPUZIP_FULL_SWEEP=1` too: a tar.gz holding a .gz is then
rescued at 120 places instead of 30, which catches the rarer ways a .gz inside can be taken for
the stream around it (about 40 s more).

Before a release, or after a change to how archives are read or written, run the real-world
campaign in [docs/testing.md](docs/testing.md): public and generated archives against 7-Zip,
hostile ones, fuzzing, other file systems, a busy GPU, and the A/B of `bench/realworld.py`
between the last release and the change (0 silent errors is the bar).

A change to rescue should also pass the damage that happens in practice, in
[research/recovery](research/recovery): `realistic.py` breaks six files in eleven ways and
`evaluate.py` compares gpuzip with gzip, tar, 7-Zip and Python's zipfile. A change to gpuinflate
should pass its fuzzer (`gpuinflate/fuzz_all.ps1`, `gi_test fuzz`).

## Measuring

Speed claims come from `bench/bench.py` (ZIP) and `bench/bench_gz.py` (gzip): median of three
interleaved runs, warm file cache, process start included, every output checked. Their results
are kept in `bench/results*.json`; `python bench/charts.py` redraws the charts in `docs/img`
from them, and `bench/terminal.py` draws a terminal session from its real output. Say what
machine a number comes from, and keep the cases where gpuzip loses.

## Style

- Code comments and documentation in plain English, saying why rather than what, in the style of
  the files around them.
- `cargo fmt` is not enforced; match the surrounding code.
- The human output is for people and may change; the `--json` reports are for scripts, so keep
  their fields stable and add rather than rename.
- No proprietary binaries in the repository: nvCOMP is NVIDIA's and stays out (`.gitignore`
  covers the build folders).

## Pull requests

One change per pull request, with what it changes, why, and how it was checked (the commands
and their results). New behavior comes with a test that fails without it.

## Releasing

Set the version in `Cargo.toml` and in the README's install commands (the packages' names), give
it a section in `CHANGELOG.md`, and push a tag `v<version>` on main. [release.yml](.github/workflows/release.yml)
builds and checks the packages for Windows, Linux and macOS and drafts the release with them;
publish it by hand after reading it over ([docs/building.md](docs/building.md#release-packages)). Then
bring the package managers up to the new version: `bucket/gpuzip.json` (Scoop) and
`Formula/gpuzip.rb` (Homebrew) take its URLs and the SHA-256s in its `SHA256SUMS`, and winget a
new folder in `packaging/winget`, sent to microsoft/winget-pkgs as a pull request.
