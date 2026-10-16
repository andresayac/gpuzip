# Runs gpuinflate on every test stream and prints one line per path (CPU sequential, CPU
# speculative, GPU last run). Usage: run_all.ps1 [inputs-folder]
param([string]$Data = "$env:TEMP\gpuzip-bench\inflate")
$exe = Join-Path $PSScriptRoot "gpuinflate.exe"
$cases = [ordered]@{
    "7zip-mx5" = "text.bin"; "zlib-ng-l1" = "text.bin"; "zlib-ng-l6" = "text.bin"; "zlib-ng-l9" = "text.bin";
    "libdeflate-l12" = "text.bin"; "mixed" = "mixed.bin"; "repeats" = "repeats.bin"; "small" = "small.bin"
}
foreach ($name in $cases.Keys) {
    $stream = Join-Path $Data "$name.deflate"
    if (-not (Test-Path $stream)) { continue }
    "== $name"
    $lines = & $exe $stream (Join-Path $Data $cases[$name]) 2>&1 | ForEach-Object { "$_" }
    $lines | Where-Object { $_ -match "sequential|CPU spec|failed|expected" }
    $lines | Where-Object { $_ -match "GPU spec" } | Select-Object -Last 1
}
