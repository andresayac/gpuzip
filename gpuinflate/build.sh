#!/bin/sh
# Builds the libraries on Linux and macOS (build.cmd does it on Windows):
#   libgzrescue.so / libgzrescue.dylib   recovery of damaged gzip and ZIP files, CPU only: any C++20 compiler
#   libgpuinflate.so and gi_test          Linux only, with the CUDA toolkit's nvcc (macOS has no CUDA)
#
#   ./build.sh            libgzrescue, and gpuinflate for this machine's GPU if nvcc is there
#   ./build.sh release    gpuinflate for every GPU from Turing (RTX 20) on, plus PTX for newer ones
#   ./build.sh rescue     libgzrescue only
# The CUDA runtime is linked statically: libgpuinflate.so needs only the NVIDIA driver.
# CXX picks the C++ compiler (default c++), NVCC the CUDA compiler (default nvcc on PATH, then
# $CUDA_PATH/bin/nvcc, then /usr/local/cuda/bin/nvcc).
set -e
cd "$(dirname "$0")"
CXX=${CXX:-c++}

if [ "$(uname -s)" = Darwin ]; then
    rescue_lib=libgzrescue.dylib
    shared=-dynamiclib
else
    rescue_lib=libgzrescue.so
    shared=-shared
fi
"$CXX" -O2 -std=c++20 -fPIC -fvisibility=hidden $shared -DGR_BUILD rescue.cpp -o "$rescue_lib"
echo "built $rescue_lib"

[ "$1" = rescue ] && exit 0
if [ "$(uname -s)" != Linux ]; then
    echo "no CUDA on $(uname -s): gpuinflate not built"
    exit 0
fi
NVCC=${NVCC:-$(command -v nvcc || true)}
[ -z "$NVCC" ] && [ -n "$CUDA_PATH" ] && [ -x "$CUDA_PATH/bin/nvcc" ] && NVCC=$CUDA_PATH/bin/nvcc
[ -z "$NVCC" ] && [ -x /usr/local/cuda/bin/nvcc ] && NVCC=/usr/local/cuda/bin/nvcc
if [ -z "$NVCC" ]; then
    if [ "$1" = release ]; then
        echo "nvcc not found: install the CUDA toolkit or set NVCC" >&2
        exit 1
    fi
    echo "nvcc not found: gpuinflate not built (gzip files are decoded on the CPU without it)"
    exit 0
fi

arch=-arch=native
if [ "$1" = release ]; then
    arch="-gencode arch=compute_75,code=sm_75 -gencode arch=compute_80,code=sm_80 -gencode arch=compute_86,code=sm_86"
    arch="$arch -gencode arch=compute_89,code=sm_89 -gencode arch=compute_120,code=[sm_120,compute_120]"
fi
if ! "$NVCC" -O3 -std=c++20 $arch -lineinfo -shared -cudart static -Xcompiler "-O2 -fPIC -fvisibility=hidden" \
        -Xptxas -v -o libgpuinflate.so gpuinflate.cu > build.log 2>&1; then
    grep -i error build.log >&2
    exit 1
fi
"$CXX" -O2 -std=c++20 gi_test.cpp -L. -lgpuinflate -Wl,-rpath,'$ORIGIN' -o gi_test
echo "built libgpuinflate.so and gi_test"
