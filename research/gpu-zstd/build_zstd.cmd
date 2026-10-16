@echo off
rem Builds libzstd (zstd.lib) from the zstd sources the zstd-sys crate vendors, so the CUDA
rem program can call ZSTD_compressSequences. Run once; build.cmd uses the result.
setlocal
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1 || exit /b 1
cd /d "%~dp0"
for /d %%d in ("%USERPROFILE%\.cargo\registry\src\index.crates.io-*") do set ZSTD=%%d\zstd-sys-2.1.1+zstd.1.5.7\zstd\lib
if not exist "%ZSTD%\zstd.h" (echo zstd sources not found under %USERPROFILE%\.cargo\registry\src & exit /b 1)
echo %ZSTD%> zstd_dir.txt
if exist zstd_obj rmdir /s /q zstd_obj
mkdir zstd_obj
cd zstd_obj
for %%d in (common compress decompress) do (
    for %%f in ("%ZSTD%\%%d\*.c") do (
        cl /nologo /O2 /MT /c /DZSTD_DISABLE_ASM /DZSTD_LEGACY_SUPPORT=0 /DXXH_NAMESPACE=ZSTD_ /I"%ZSTD%" /I"%ZSTD%\common" "%%f" >nul || exit /b 1
    )
)
lib /nologo /out:..\zstd.lib *.obj || exit /b 1
cd ..
rmdir /s /q zstd_obj
echo built zstd.lib from %ZSTD%
