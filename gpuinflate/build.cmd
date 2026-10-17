@echo off
rem Builds gzrescue.dll (CPU only: the C++ compiler is enough), then gpuinflate.dll and
rem gi_test.exe with nvcc and the Visual Studio C++ compiler. build.sh does it on Linux and macOS.
rem   build.cmd           this machine's GPU only (fast, for development)
rem   build.cmd release   every GPU from Turing (RTX 20) on, plus PTX for newer ones
rem   build.cmd rescue    gzrescue.dll only (no CUDA toolkit needed)
rem The CUDA runtime is linked statically: the DLL needs only the NVIDIA driver.
setlocal
rem the C++ compiler: already set up (a Developer Command Prompt), or the newest Visual Studio with it
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
where cl >nul 2>&1 || for /f "usebackq delims=" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do call "%%i\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
where cl >nul 2>&1 || (echo Visual Studio with its C++ tools not found & exit /b 1)
cd /d "%~dp0"
rem gzrescue.dll: recovery of damaged gzip and ZIP files, CPU only (no CUDA needed to build or run)
cl /nologo /O2 /EHsc /std:c++20 /MT /LD /DGR_BUILD rescue.cpp /Fe:gzrescue.dll >nul || exit /b 1
del /q rescue.obj 2>nul
echo built gzrescue.dll
if /i "%1"=="rescue" exit /b 0
set ARCH=-arch=native
if /i "%1"=="release" set ARCH=-gencode arch=compute_75,code=sm_75 -gencode arch=compute_80,code=sm_80 -gencode arch=compute_86,code=sm_86 -gencode arch=compute_89,code=sm_89 -gencode arch=compute_120,code=[sm_120,compute_120]
"%CUDA_PATH%\bin\nvcc.exe" -O3 -std=c++20 %ARCH% -lineinfo -shared -cudart static -Xcompiler "/O2 /EHsc /MT" -Xptxas -v -o gpuinflate.dll gpuinflate.cu > build.log 2>&1 || (findstr /i "error" build.log & exit /b 1)
cl /nologo /O2 /EHsc /std:c++20 /MT gi_test.cpp gpuinflate.lib /Fe:gi_test.exe >nul || exit /b 1
del /q gi_test.obj 2>nul
echo built gpuinflate.dll and gi_test.exe
