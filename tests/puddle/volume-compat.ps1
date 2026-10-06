# SPDX-License-Identifier: Apache-2.0
# volume-compat.ps1 - a workspace volume made by the previous fork release opens on the new one
# (fork-only file; gate of the puddle release and auto-bump workflows).
#
# With the OLD msb.exe: boot a sandbox with a named disk volume (created on first mount, with a
# size, as puddle's workspace volumes are), write a marker file, stop and remove the sandbox (the
# volume stays). Then, in the SAME MSB_HOME, with the NEW msb.exe: list the volume, boot a new
# sandbox with the volume mounted and read the marker back, write to it, restart, read again.
# This also has the new msb open the old msb's database and image cache.
#
# Windows PowerShell 5.1 safe (ASCII only). On a hosted Actions runner start it through
# run-outside-job.ps1 (msb create needs to leave the step's job object).
# usage: volume-compat.ps1 -OldMsb <exe> -OldLibkrunfw <dll> -NewMsb <exe> -NewLibkrunfw <dll> [-Work <dir>]
# Exit code: 0 pass, 1 fail, 2 bad usage. Nothing is deleted: the script prints the work dir.

param(
    [Parameter(Mandatory = $true)] [string]$OldMsb,
    [Parameter(Mandatory = $true)] [string]$OldLibkrunfw,
    [Parameter(Mandatory = $true)] [string]$NewMsb,
    [Parameter(Mandatory = $true)] [string]$NewLibkrunfw,
    [string]$Work = '',
    [string]$Prefix = 'vc'
)

$ErrorActionPreference = 'Continue'
foreach ($f in @($OldMsb, $OldLibkrunfw, $NewMsb, $NewLibkrunfw)) {
    if (-not (Test-Path -LiteralPath $f -PathType Leaf)) { Write-Output ('missing file: ' + $f); exit 2 }
}
if ($Work -eq '') { $Work = Join-Path $env:TEMP ('puddle-volcompat-' + [guid]::NewGuid().ToString('N').Substring(0, 8)) }
$MsbHome = Join-Path $Work 'home'
$OutDir = Join-Path $Work 'out'
New-Item -ItemType Directory -Force -Path $MsbHome, $OutDir | Out-Null
$env:MSB_HOME = $MsbHome
$Vol = $Prefix + '-data'
$script:Msb = $null

function Use-Msb {
    param([string]$Exe, [string]$Dll, [string]$Label)
    $script:Msb = (Resolve-Path -LiteralPath $Exe).Path
    $env:MSB_PATH = $script:Msb
    $env:MSB_LIBKRUNFW_PATH = (Resolve-Path -LiteralPath $Dll).Path
    Write-Output ('--- ' + $Label + ': ' + $script:Msb)
}

function Invoke-Msb {
    # cmd.exe with all output to a file, no pipes (a detached runtime would hold them open).
    param([string[]]$A, [int]$TimeoutSec = 600)
    $out = Join-Path $OutDir ([guid]::NewGuid().ToString('N') + '.txt')
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = Join-Path $env:SystemRoot 'System32\cmd.exe'
    $psi.Arguments = '/c ""' + $script:Msb + '" ' + ($A -join ' ') + ' > "' + $out + '" 2>&1 < NUL"'
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    if (-not $p.WaitForExit($TimeoutSec * 1000)) { return @{ Code = 124; Text = 'TIMEOUT after ' + $TimeoutSec + ' s' } }
    $text = ''
    if (Test-Path $out) { $text = (Get-Content -Encoding UTF8 -Path $out | Out-String).Trim() }
    return @{ Code = $p.ExitCode; Text = $text }
}

$script:Failed = $null
function Step {
    # Runs one msb call, prints it, records the first failure. Returns the output text.
    param([string]$Label, [string[]]$A, [string]$Expect = '')
    if ($script:Failed) { return '' }
    $r = Invoke-Msb $A
    Write-Output ('  ' + $Label + ': exit ' + $r.Code)
    if ($r.Text -ne '') { $r.Text -split "`n" | ForEach-Object { Write-Output ('      ' + $_.TrimEnd()) } }
    if ($r.Code -ne 0) { $script:Failed = $Label + ' exited ' + $r.Code }
    elseif ($Expect -ne '' -and $r.Text -notmatch [regex]::Escape($Expect)) { $script:Failed = $Label + ' did not print ' + $Expect }
}

function Q { param([string]$S) return '"' + $S + '"' }

$marker = 'puddle-volume-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
Use-Msb $OldMsb $OldLibkrunfw 'old msb'
Step 'old: --version' @('--version')
Step 'old: create with the volume' @('create', 'alpine', '--name', ($Prefix + '-old'), '--mount-named', ($Vol + ':/data:kind=disk,size=1G'))
Step 'old: write marker' @('exec', ($Prefix + '-old'), '--', 'sh', '-c', (Q ('echo ' + $marker + ' > /data/marker; mkdir -p /data/dir; echo nested > /data/dir/f; sync; cat /data/marker'))) $marker
Step 'old: stop' @('stop', ($Prefix + '-old'))
Step 'old: rm' @('rm', '-f', ($Prefix + '-old'))

Use-Msb $NewMsb $NewLibkrunfw 'new msb, same MSB_HOME'
Step 'new: --version' @('--version')
Step 'new: ls (opens the old database)' @('ls')
Step 'new: volume ls' @('volume', 'ls') $Vol
Step 'new: create with the old volume' @('create', 'alpine', '--name', ($Prefix + '-new'), '--mount-named', ($Vol + ':/data:kind=disk,size=1G'))
Step 'new: read marker' @('exec', ($Prefix + '-new'), '--', 'sh', '-c', (Q 'cat /data/marker /data/dir/f; echo appended >> /data/marker')) $marker
Step 'new: stop' @('stop', ($Prefix + '-new'))
Step 'new: start' @('start', ($Prefix + '-new'))
Step 'new: read after restart' @('exec', ($Prefix + '-new'), '--', 'sh', '-c', (Q 'cat /data/marker')) 'appended'
Invoke-Msb @('stop', ($Prefix + '-new')) 120 | Out-Null
Invoke-Msb @('rm', '-f', ($Prefix + '-new')) 120 | Out-Null
Invoke-Msb @('volume', 'rm', $Vol) 120 | Out-Null

Write-Output ('work dir (delete when done): ' + $Work)
if ($script:Failed) { Write-Output ('VOLUME COMPAT: FAIL (' + $script:Failed + ')'); exit 1 }
Write-Output 'VOLUME COMPAT: PASS (volume from the old msb mounted, read and written by the new msb)'
exit 0
