# Runs gi_test on the prototype's eight streams (see ..\research\gpu-inflate\make_*inputs.py).
#   run_all.ps1 [-Data DIR] [-Extra "--seg-in 1024"]
param([string]$Data = "$env:TEMP\gpuzip-bench\inflate", [string]$Extra = "")
$exe = Join-Path $PSScriptRoot "gi_test.exe"
$cases = @(
    @("7zip-mx5", "text.bin"), @("zlib-ng-l6", "text.bin"), @("zlib-ng-l9", "text.bin"),
    @("zlib-ng-l1", "text.bin"), @("libdeflate-l12", "libdeflate-l12.bin"), @("mixed", "mixed-expect.bin"),
    @("repeats", "repeats.bin"), @("small", "small.bin"))
foreach ($c in $cases) {
    $args = @("run", (Join-Path $Data "$($c[0]).deflate"), (Join-Path $Data $c[1]), "--runs", "3") + ($Extra -split ' ' | Where-Object { $_ })
    $lines = & $exe @args 2>&1 | ForEach-Object { "$_" }
    $checked = $lines | Select-String "GPU, checked" | Select-Object -Last 1
    $best = $lines | Select-String "GB/s" | ForEach-Object { [double]($_.Line -replace '^.*= *([\d.]+) GB/s.*$', '$1') } | Measure-Object -Maximum
    $ok = if ($checked -and $checked.Line -match " OK ") { "OK" } else { "FAIL" }
    "{0,-15} {1,-4} best {2,5:N2} GB/s   {3}" -f $c[0], $ok, $best.Maximum, (($lines | Select-String "no sink" | Select-Object -Last 1).Line -replace '^.*\[', '[')
    if ($ok -ne "OK") { $lines | Select-Object -Last 6 }
}
