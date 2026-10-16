# Runs gpuzstd on a few corpus files with different settings and prints one line per run.
#   variants.ps1 [-Files binaries.tar,logs.txt] [-Variants @(@("--seg","1024"), @("--pred","2"))]
param(
    [string[]]$Files = @("binaries.tar", "logs.txt"),
    [object[]]$Variants = @(@(), @("--seg", "2048"), @("--seg", "1024"), @("--pred", "2"), @("--orders", "3"), @("--passes", "1"), @("--sample", "1")),
    [string]$Corpus = "$env:TEMP\gpuzip-bench\corpus"
)
$exe = Join-Path $PSScriptRoot "gpuzstd.exe"
foreach ($v in $Variants) {
    $label = if ($v.Count) { $v -join " " } else { "defaults" }
    foreach ($f in $Files) {
        $lines = & $exe (Join-Path $Corpus $f) @v 2>&1 | ForEach-Object { "$_" }
        $ratio = ($lines | Select-String "ratio ([\d.]+)").Matches[0].Groups[1].Value
        $gpu = ($lines | Select-String "^GPU (\d+) ms").Matches[0].Groups[1].Value
        $detail = (($lines | Select-String "^GPU") -replace '^GPU \d+ ms = [\d.]+ GB/s\s+', '') -replace '; CPU.*$', ''
        $ok = if ($lines -match "decodes to the input") { "ok" } else { "FAIL" }
        "{0,-22} {1,-13} ratio {2,8}  GPU {3,5} ms  {4}  {5}" -f $label, $f, $ratio, $gpu, $detail, $ok
    }
}
