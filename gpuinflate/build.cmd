@echo off
rem Builds gpuinflate.dll (and gi_test.exe) with nvcc and the Visual Studio C++ compiler.
rem   build.cmd           this machine's GPU only (fast, for development)
rem   build.cmd release   every GPU from Turing (RTX 20) on, plus PTX for newer ones
rem The CUDA runtime is linked statically: the DLL needs only the NVIDIA driver.
setlocal
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
cd /d "%~dp0"
set ARCH=-arch=native
if /i "%1"=="release" set ARCH=-gencode arch=compute_75,code=sm_75 -gencode arch=compute_80,code=sm_80 -gencode arch=compute_86,code=sm_86 -gencode arch=compute_89,code=sm_89 -gencode arch=compute_120,code=[sm_120,compute_120]
"%CUDA_PATH%\bin\nvcc.exe" -O3 -std=c++20 %ARCH% -lineinfo -shared -cudart static -Xcompiler "/O2 /EHsc /MT" -Xptxas -v -o gpuinflate.dll gpuinflate.cu > build.log 2>&1 || (findstr /i "error" build.log & exit /b 1)
cl /nologo /O2 /EHsc /std:c++20 /MT gi_test.cpp gpuinflate.lib /Fe:gi_test.exe >nul || exit /b 1
del /q gi_test.obj 2>nul
echo built gpuinflate.dll and gi_test.exe
rem gzrescue.dll: recovery of damaged gzip files, CPU only (no CUDA needed to build or run)
cl /nologo /O2 /EHsc /std:c++20 /MT /LD /DGR_BUILD rescue.cpp /Fe:gzrescue.dll >nul || exit /b 1
del /q rescue.obj 2>nul
echo built gzrescue.dll
