# Inference accuracy and coverage over settings, on the damaged code tar and logs of realistic.py.
#   infer_sweep.ps1 [-SoloAgainst "6:2","8:4"] [-Margins "10:60","14:60"]
# SOLO: how far one context alone must favour the value; AGAINST: how far none may favour another.
param([string[]]$SoloAgainst = @("4:1", "6:2", "8:4"), [string[]]$Margins = @("10:60", "14:60"), [string]$Dir = "$env:TEMP\gpuzip-bench\realistic")
$py = Join-Path $PSScriptRoot "infer_accuracy.py"
$cases = @(@("code.bad_sectors.tar.gz", "code.tar"), @("code.flash_ff.tar.gz", "code.tar"), @("code.torrent_holes.tar.gz", "code.tar"),
           @("logs.bad_sectors.txt.gz", "logs.txt"), @("logs.flash_ff.txt.gz", "logs.txt"))
foreach ($sa in $SoloAgainst) {
    $f = $sa.Split(":")
    $env:GR_INFER_SOLO = $f[0]
    $env:GR_INFER_AGAINST = $f[1]
    foreach ($case in $cases) {
        "--- solo $($f[0]), against $($f[1]), $($case[0])"
        python $py $Dir (Join-Path $Dir $case[0]) (Join-Path $Dir $case[1]) @Margins
    }
}
Remove-Item Env:GR_INFER_SOLO, Env:GR_INFER_AGAINST
