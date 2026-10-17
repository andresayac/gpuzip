#!/bin/sh
# Builds gzrescue (CPU only) from the library source in ../../gpuinflate/rescue.cpp, on Linux
# and macOS (build.cmd does it on Windows). CXX picks the compiler (default c++).
set -e
cd "$(dirname "$0")"
"${CXX:-c++}" -O2 -std=c++20 -DGR_STATIC gzrescue.cpp ../../gpuinflate/rescue.cpp -o gzrescue
echo "built gzrescue"
