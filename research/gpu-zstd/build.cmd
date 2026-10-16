@echo off
rem Builds the gpu-zstd experiments with nvcc (CUB comes with the CUDA toolkit).
rem   gpumatch.exe   match finding alone, with statistics and candidate dumps
rem   gpuzstd.exe    match finding + optimal parse on the GPU, zstd's entropy coder, .zst output
rem gpuzstd needs zstd.lib from build_zstd.cmd.
setlocal
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1 || exit /b 1
cd /d "%~dp0"
set NVCC="%CUDA_PATH%\bin\nvcc.exe" -O3 -std=c++20 -arch=native -lineinfo -Xcompiler "/O2 /EHsc /MT /Zc:preprocessor"
%NVCC% -o gpumatch.exe match.cu > build.log 2>&1 || (findstr /i "error" build.log & exit /b 1)
echo built gpumatch.exe
if not exist zstd.lib (echo zstd.lib missing: run build_zstd.cmd & exit /b 1)
set /p ZSTD=<zstd_dir.txt
%NVCC% -I"%ZSTD%" -o gpuzstd.exe gpuzstd.cu zstd.lib >> build.log 2>&1 || (findstr /i "error" build.log & exit /b 1)
echo built gpuzstd.exe
