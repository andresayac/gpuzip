# Builds gpuinflate with different register caps (blocks of 256 threads per SM) and times the
# GPU path on two streams, to pick the cap.
param([string]$Data = "$env:TEMP\gpuzip-bench\inflate")
$vc = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
foreach ($b in 2, 3, 4, 5, 6) {
    $exe = Join-Path $PSScriptRoot "gpuinflate_b$b.exe"
    cmd /c "`"$vc`" >nul 2>&1 && cd /d `"$PSScriptRoot`" && `"%CUDA_PATH%\bin\nvcc.exe`" -O3 -std=c++20 -arch=sm_120 -DGI_DECODE_BLOCKS_PER_SM=$b -Xcompiler `"/O2 /EHsc`" -o `"$exe`" gpuinflate.cu >nul 2>&1"
    foreach ($s in "7zip-mx5", "zlib-ng-l1") {
        $line = & $exe (Join-Path $Data "$s.deflate") (Join-Path $Data "text.bin") 2>&1 | ForEach-Object { "$_" } |
            Select-String "GPU spec" | Select-Object -Last 1
        "{0} blocks/SM  {1,-11} {2}" -f $b, $s, ($line.Line -replace '^GPU speculative:\s+', '' -replace '\s+starts.*$', '')
    }
    Remove-Item "$PSScriptRoot\gpuinflate_b$b.*" -ErrorAction SilentlyContinue
}
