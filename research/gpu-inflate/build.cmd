@echo off
rem Builds gpuinflate.exe with nvcc and the Visual Studio C++ compiler (needed by nvcc on Windows).
setlocal
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
cd /d "%~dp0"
"%CUDA_PATH%\bin\nvcc.exe" -O3 -std=c++20 -arch=sm_120 -lineinfo -Xcompiler "/O2 /EHsc" -o gpuinflate.exe gpuinflate.cu
