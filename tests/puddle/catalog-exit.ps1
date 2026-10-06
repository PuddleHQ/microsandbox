# SPDX-License-Identifier: Apache-2.0
# catalog-exit.ps1 - msb processes that open the SQLite catalog while others exit (fork-only file).
#
# Each round starts -Par `msb sandbox ls` processes at once against one MSB_HOME. Every one opens
# the catalog (WAL, migration check, install lease), prints, and exits with its pool still open, so
# at any moment some are opening while others are exiting. The case PASSES when every process
# exits 0; a "disk I/O error" or other catalog failure is printed with the process's output.
#
#   powershell -ExecutionPolicy Bypass -File catalog-exit.ps1 -Msb <msb.exe> [-Rounds 100] [-Par 8]
# Exit code: 0 all passed, 1 some process failed, 2 bad usage. Windows PowerShell 5.1 safe (ASCII only).

param(
    [Parameter(Mandatory = $true)] [string]$Msb,
    [int]$Rounds = 100,
    [int]$Par = 8,
    [string]$Work = ''
)
$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $Msb -PathType Leaf)) { Write-Output "no msb.exe at $Msb"; exit 2 }
$Msb = (Resolve-Path -LiteralPath $Msb).Path
if ($Work -eq '') { $Work = Join-Path $env:TEMP ('puddle-catalog-exit-' + [guid]::NewGuid().ToString('N').Substring(0, 8)) }
$MsbHome = Join-Path $Work 'home'
New-Item -ItemType Directory -Force -Path $MsbHome, (Join-Path $Work 'out') | Out-Null
$env:MSB_HOME = $MsbHome

# The first run creates and migrates the catalog on its own.
& $Msb sandbox ls 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { Write-Output "first open failed with $LASTEXITCODE"; exit 1 }

$failed = 0
$total = 0
$sw = [Diagnostics.Stopwatch]::StartNew()
for ($r = 1; $r -le $Rounds; $r++) {
    $procs = @()
    for ($i = 0; $i -lt $Par; $i++) {
        $o = Join-Path $Work ("out\r{0}-{1}.out" -f $r, $i)
        $e = Join-Path $Work ("out\r{0}-{1}.err" -f $r, $i)
        $p = Start-Process -FilePath $Msb -ArgumentList @('sandbox', 'ls') -NoNewWindow -PassThru `
            -RedirectStandardOutput $o -RedirectStandardError $e
        $procs += , @($p, $o, $e)
    }
    foreach ($x in $procs) {
        $x[0].WaitForExit()
        $total++
        if ($x[0].ExitCode -ne 0) {
            $failed++
            if ($failed -le 8) {
                Write-Output ("round {0}: exit {1}: {2}" -f $r, $x[0].ExitCode, ((Get-Content -Raw -LiteralPath $x[2]) -replace '\s+', ' '))
            }
        }
    }
}
Write-Output ("catalog-exit: {0} of {1} msb processes failed in {2:N1} s (par {3})" -f $failed, $total, $sw.Elapsed.TotalSeconds, $Par)
if ($failed -gt 0) { exit 1 }
exit 0
