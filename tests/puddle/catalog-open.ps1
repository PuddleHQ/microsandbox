# SPDX-License-Identifier: Apache-2.0
# catalog-open.ps1 - open msb's SQLite catalog while real sandbox runtimes exit (fork-only file).
#
# Starts -Opener (the crates/db test binary open_after_process_exit, test open_loop_against_running_msb),
# which opens the catalog from a fresh connection pair again and again, then boots and stops
# -Rounds x -Par alpine sandboxes with msb.exe. Every `msb stop` ends a sandbox runtime process
# that holds the catalog. The case PASSES when the opener saw no open failure ("disk I/O error"
# with extended code 1546 is the one this chases).
#
# Run it through run-outside-job.ps1 on a hosted runner (create/start need breakaway from the
# step's job object). Windows PowerShell 5.1 safe (ASCII only).
#   catalog-open.ps1 -Msb <msb.exe> -Libkrunfw <dll> -Opener <test exe> [-Rounds 10] [-Par 4] [-Work <dir>]
# Exit code: 0 pass, 1 fail, 2 bad usage.

param(
    [Parameter(Mandatory = $true)] [string]$Msb,
    [Parameter(Mandatory = $true)] [string]$Libkrunfw,
    [Parameter(Mandatory = $true)] [string]$Opener,
    [int]$Rounds = 10,
    [int]$Par = 4,
    [string]$Work = ''
)
$ErrorActionPreference = 'Stop'
foreach ($f in @($Msb, $Libkrunfw, $Opener)) { if (-not (Test-Path -LiteralPath $f -PathType Leaf)) { Write-Output "missing file $f"; exit 2 } }
$Msb = (Resolve-Path -LiteralPath $Msb).Path
$Opener = (Resolve-Path -LiteralPath $Opener).Path
if ($Work -eq '') { $Work = Join-Path $env:TEMP ('puddle-catalog-open-' + [guid]::NewGuid().ToString('N').Substring(0, 8)) }
$MsbHome = Join-Path $Work 'home'
$OutDir = Join-Path $Work 'out'
New-Item -ItemType Directory -Force -Path $MsbHome, $OutDir | Out-Null
$env:MSB_HOME = $MsbHome
$env:MSB_PATH = $Msb
$env:MSB_LIBKRUNFW_PATH = (Resolve-Path -LiteralPath $Libkrunfw).Path

function Invoke-Cmd {
    # Through cmd.exe with all output in a file: a sandbox runtime that inherits a pipe would
    # keep "msb create" from returning.
    param([string]$Args2, [int]$TimeoutSec = 300)
    $out = Join-Path $OutDir ([guid]::NewGuid().ToString('N') + '.txt')
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = Join-Path $env:SystemRoot 'System32\cmd.exe'
    $psi.Arguments = '/c ""' + $Msb + '" ' + $Args2 + ' > "' + $out + '" 2>&1 < NUL"'
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    return [pscustomobject]@{ P = $p; Out = $out }
}
function Wait-Cmd { param($R, [int]$TimeoutSec = 300) if (-not $R.P.WaitForExit($TimeoutSec * 1000)) { return 124 } else { return $R.P.ExitCode } }

# Create the catalog and fetch the image before the opener starts (it never creates the catalog).
$r = Invoke-Cmd 'pull alpine' 600
if ((Wait-Cmd $r 600) -ne 0) { Get-Content $r.Out | Write-Output; Write-Output 'pull failed'; exit 1 }
$db = Join-Path $MsbHome 'db\msb.db'
if (-not (Test-Path -LiteralPath $db)) { Write-Output "no catalog at $db after pull"; exit 1 }

$stop = Join-Path $Work 'stop'
$env:MSB_DB_OPEN_LOOP = $db
$env:MSB_DB_OPEN_LOOP_STOP = $stop
$openerOut = Join-Path $Work 'opener.out'
$openerErr = Join-Path $Work 'opener.err'
$loopProc = Start-Process -FilePath $Opener -ArgumentList @('--exact', 'open_loop_against_running_msb', '--nocapture') -NoNewWindow -PassThru `
    -RedirectStandardOutput $openerOut -RedirectStandardError $openerErr
Remove-Item Env:MSB_DB_OPEN_LOOP, Env:MSB_DB_OPEN_LOOP_STOP

$bad = 0
$sw = [Diagnostics.Stopwatch]::StartNew()
for ($round = 1; $round -le $Rounds; $round++) {
    $runs = @()
    for ($i = 0; $i -lt $Par; $i++) {
        $name = 'co-' + $round + '-' + $i
        $runs += [pscustomobject]@{ Name = $name; R = (Invoke-Cmd ('create alpine --name ' + $name + ' --cpus 1 --replace')) }
    }
    foreach ($x in $runs) {
        $code = Wait-Cmd $x.R
        if ($code -ne 0) { $bad++; Write-Output ("create " + $x.Name + " exit " + $code + ": " + ((Get-Content -Raw $x.R.Out) -replace '\s+', ' ')) }
    }
    foreach ($x in $runs) {
        Wait-Cmd (Invoke-Cmd ('stop ' + $x.Name)) 120 | Out-Null
        Wait-Cmd (Invoke-Cmd ('rm -f ' + $x.Name)) 120 | Out-Null
    }
    Write-Output ("round {0} done ({1:N0} s)" -f $round, $sw.Elapsed.TotalSeconds)
}
New-Item -ItemType File -Force -Path $stop | Out-Null
if (-not $loopProc.WaitForExit(120000)) { $loopProc.Kill(); Write-Output 'opener did not stop'; exit 1 }
Get-Content $openerErr | Select-Object -Last 40 | Write-Output
Get-Content $openerOut | Select-String 'test result|panicked' | Write-Output
Write-Output ("catalog-open: opener exit {0}, {1} sandbox creates failed" -f $loopProc.ExitCode, $bad)
if ($loopProc.ExitCode -ne 0 -or $bad -gt 0) { exit 1 }
exit 0
