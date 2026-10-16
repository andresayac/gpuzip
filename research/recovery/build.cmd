@echo off
rem Builds gzrescue.exe (CPU only) with the Visual Studio C++ compiler.
setlocal
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1 || exit /b 1
cd /d "%~dp0"
cl /nologo /O2 /EHsc /std:c++20 /MT gzrescue.cpp /Fe:gzrescue.exe > build.log 2>&1 || (type build.log & exit /b 1)
del /q gzrescue.obj 2>nul
echo built gzrescue.exe
