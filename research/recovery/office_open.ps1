# Opens one document in its Office application through COM: hidden, alerts off, read only, not
# added to the recent files. First as it is; if the application refuses it, through its own
# repair (Word's OpenAndRepair, Excel's CorruptLoad, PowerPoint's Open2007 with repair). Writes
# the result as JSON: clean, repaired (opened only through the repair) or refused.
#
# Excel with alerts off does not refuse a damaged workbook: it repairs it without asking, and
# leaves a repair log (error*.xml) in TEMP, which is how the repair is seen. Word and PowerPoint
# refuse with an error. Checked on documents with known faults: a Word document that opens here
# also opens with alerts on, with no prompt.
#
# The application's PID, from its window, goes to $PidFile as soon as it is known, so the caller
# can end it on a timeout. An instance that was already running (PowerPoint keeps only one, and
# automation attaches to it) is never quit.
param(
    [Parameter(Mandatory)][ValidateSet('word', 'excel', 'powerpoint')][string]$App,
    [Parameter(Mandatory)][string]$File,
    [Parameter(Mandatory)][string]$PidFile,
    [Parameter(Mandatory)][string]$ResultFile,
    [string]$Preexisting = '',  # PIDs of the application running before, comma separated
    [switch]$Alerts             # Word with alerts on: a prompt blocks the call, the caller's timeout sees it
)
$ErrorActionPreference = 'Stop'
Add-Type -Namespace W -Name U -MemberDefinition @'
[DllImport("user32.dll", SetLastError=true)] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
[DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
'@
$pre = @($Preexisting -split ',' | Where-Object { $_ } | ForEach-Object { [int]$_ })
$exe = @{ word = 'WINWORD'; excel = 'EXCEL'; powerpoint = 'POWERPNT' }[$App]
$M = [Type]::Missing
$res = [ordered]@{ app = $App; file = $File; result = $null; first_error = $null; repair_error = $null; details = @{}; pid = $null; attached = $false }

function TempErrorLogs {
    $dirs = @($env:TEMP, [Environment]::GetEnvironmentVariable('TEMP', 'User')) | Where-Object { $_ } | Select-Object -Unique
    foreach ($d in $dirs) { Get-ChildItem -LiteralPath ([Environment]::ExpandEnvironmentVariables($d)) -Filter 'error*.xml' -File -ErrorAction SilentlyContinue }
}

# late bound (IDispatch): the typed interop wants by-reference arguments; Missing = left out
function Call($obj, [string]$method, [object[]]$a) {
    try {
        return [System.__ComObject].InvokeMember($method, [Reflection.BindingFlags]::InvokeMethod, $null, $obj, $a)
    } catch {
        $e = $_.Exception; while ($e.InnerException) { $e = $e.InnerException }
        throw $e
    }
}

function OpenDoc($office, [bool]$repair) {
    switch ($App) {
        'word' {
            # FileName, ConfirmConversions, ReadOnly, AddToRecentFiles, PasswordDocument, PasswordTemplate, Revert,
            # WritePasswordDocument, WritePasswordTemplate, Format, Encoding, Visible, OpenAndRepair, DocumentDirection, NoEncodingDialog
            return Call $office.Documents 'Open' @($File, $false, $true, $false, $M, $M, $false, $M, $M, 0, $M, $false, $repair, $M, $true)
        }
        'excel' {
            # Filename, UpdateLinks, ReadOnly, Format, Password, WriteResPassword, IgnoreReadOnlyRecommended, Origin,
            # Delimiter, Editable, Notify, Converter, AddToMru, Local, CorruptLoad (0 normal, 1 repair)
            return Call $office.Workbooks 'Open' @($File, 0, $true, $M, $M, $M, $true, $M, $M, $false, $false, $M, $false, $M, $(if ($repair) { 1 } else { 0 }))
        }
        'powerpoint' {
            if ($repair) { return Call $office.Presentations 'Open2007' @($File, -1, 0, 0, -1) }  # ReadOnly, Untitled, WithWindow, OpenAndRepair
            return Call $office.Presentations 'Open' @($File, -1, 0, 0)
        }
    }
}

# what the application found in it: that it opened as a document, and what it holds
function Describe($doc) {
    switch ($App) {
        'word' { return @{ paragraphs = $doc.Paragraphs.Count; characters = $doc.Characters.Count; save_format = $doc.SaveFormat } }
        'excel' {
            $cells = 0; $names = @()
            foreach ($ws in $doc.Worksheets) { $names += $ws.Name; $cells += [int]$ws.UsedRange.Cells.Count }
            return @{ sheets = $names; used_cells = $cells; file_format = $doc.FileFormat }
        }
        'powerpoint' {
            $shapes = 0; foreach ($s in $doc.Slides) { $shapes += $s.Shapes.Count }
            return @{ slides = $doc.Slides.Count; shapes = $shapes }
        }
    }
}

function CloseDoc($doc) {
    switch ($App) {
        'word' { [void](Call $doc 'Close' @(0)) }
        'excel' { [void](Call $doc 'Close' @($false)) }
        'powerpoint' { $doc.Close() }
    }
}

$office = $null
try {
    $office = New-Object -ComObject "$(@{ word = 'Word'; excel = 'Excel'; powerpoint = 'PowerPoint' }[$App]).Application"
    $hwnd = [IntPtr]::Zero
    switch ($App) {
        'word' {
            # Word has no window handle property: its window is found by a caption set for it
            $cap = "gpuzip-check-" + [guid]::NewGuid().ToString('N')
            $office.Caption = $cap
            $hwnd = [W.U]::FindWindow('OpusApp', $cap)
        }
        'excel' { $hwnd = [IntPtr][long]$office.Hwnd }
        'powerpoint' { $hwnd = [IntPtr][long]$office.HWND }
    }
    $procId = 0
    if ($hwnd -ne [IntPtr]::Zero) { [void][W.U]::GetWindowThreadProcessId($hwnd, [ref]$procId) }
    if ($procId -eq 0) {
        $new = @(Get-Process -Name $exe -ErrorAction SilentlyContinue | Where-Object { $pre -notcontains $_.Id })
        if ($new.Count -eq 1) { $procId = $new[0].Id }
    }
    $res.pid = [int]$procId
    $res.attached = ($pre -contains [int]$procId) -or ($App -eq 'powerpoint' -and $office.Presentations.Count -gt 0)
    "$procId $([int]$res.attached)" | Set-Content -LiteralPath $PidFile -Encoding ascii

    switch ($App) {
        'word' { $office.Visible = $false; $office.DisplayAlerts = $(if ($Alerts) { -1 } else { 0 }); $office.AutomationSecurity = 3; $office.Options.UpdateLinksAtOpen = $false }
        'excel' { $office.Visible = $false; $office.DisplayAlerts = $false; $office.AskToUpdateLinks = $false; $office.EnableEvents = $false; $office.AutomationSecurity = 3 }
        'powerpoint' { $office.DisplayAlerts = 1; $office.AutomationSecurity = 3 }
    }

    $logsBefore = @(TempErrorLogs | ForEach-Object FullName)
    $doc = $null
    try { $doc = OpenDoc $office $false } catch { $res.first_error = $_.Exception.Message }
    if ($doc) {
        $res.result = 'clean'
        $res.details = Describe $doc
        if ($App -eq 'excel') {
            $newLogs = @(TempErrorLogs | Where-Object { $logsBefore -notcontains $_.FullName })
            if ($newLogs.Count -gt 0) {
                $res.result = 'repaired'
                $res.details.repair_log = @($newLogs | ForEach-Object { Get-Content -LiteralPath $_.FullName -Raw -Encoding UTF8 })
            }
        }
        CloseDoc $doc
    } else {
        $logsBefore = @(TempErrorLogs | ForEach-Object FullName)
        try { $doc = OpenDoc $office $true } catch { $res.repair_error = $_.Exception.Message }
        if ($doc) {
            $res.result = 'repaired'
            $res.details = Describe $doc
            if ($App -eq 'excel') {
                $res.details.repair_log = @(TempErrorLogs | Where-Object { $logsBefore -notcontains $_.FullName } | ForEach-Object { Get-Content -LiteralPath $_.FullName -Raw -Encoding UTF8 })
            }
            CloseDoc $doc
        } else {
            $res.result = 'refused'
        }
    }
} catch {
    $res.result = 'error'
    $res.first_error = "harness: " + $_.Exception.Message
} finally {
    if ($office -and -not $res.attached) { try { $office.Quit() } catch {} }
    if ($office) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($office) }
    [GC]::Collect(); [GC]::WaitForPendingFinalizers()
    $res | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $ResultFile -Encoding UTF8
}
