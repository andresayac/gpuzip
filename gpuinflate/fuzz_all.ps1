# Fuzzes gpuinflate.dll with every stream from make_fuzz_inputs.py, with tiny segments (many
# segment boundaries) and with the default ones.
#   fuzz_all.ps1 [-Data DIR] [-Iterations N] [-Seed S]
param([string]$Data = "$env:TEMP\gpuzip-bench\fuzz", [int]$Iterations = 300, [int]$Seed = 1)
$exe = Join-Path $PSScriptRoot "gi_test.exe"
$failed = 0
Push-Location $Data  # disagreements are saved here
foreach ($f in Get-ChildItem (Join-Path $Data "*.deflate") | Where-Object { $_.Name -notlike "fuzz-fail*" }) {
    foreach ($seg in 16, 0) {
        $lines = & $exe fuzz $f.FullName $Iterations --seed $Seed --seg-in $seg 2>&1 | ForEach-Object { "$_" }
        $sum = $lines | Select-String "iterations in" | Select-Object -Last 1
        "{0,-18} seg {1,-3} {2}" -f $f.Name, $seg, $sum.Line
        $lines | Select-String "DISAGREES|GPU error" | ForEach-Object { "    " + $_.Line }
        if ($LASTEXITCODE -ne 0) { $failed++ }
    }
}
Pop-Location
"failed runs: $failed"
