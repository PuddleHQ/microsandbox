# SPDX-License-Identifier: Apache-2.0
# run-outside-job.ps1 - run a PowerShell script outside the Actions step's job object (fork-only file).
#
# msb create/start launch the VM runtime with CREATE_BREAKAWAY_FROM_JOB, and a hosted runner's
# step job object doesn't allow breakaway ("Access is denied. (os error 5)"). A process started
# through WMI Win32_Process.Create is not in that job. Its environment is the user's default one,
# not the step's, so pass every path as an argument.
#
# usage: run-outside-job.ps1 -Script <file.ps1> -Log <file> [-TimeoutSec 1500] -- <script args...>
# Streams the log while the script runs and exits with the script's exit code (124 on timeout).

param(
    [Parameter(Mandatory = $true)] [string]$Script,
    [Parameter(Mandatory = $true)] [string]$Log,
    [int]$TimeoutSec = 1500,
    [Parameter(ValueFromRemainingArguments = $true)] [string[]]$ScriptArgs
)

$ErrorActionPreference = 'Stop'
$Script = (Resolve-Path -LiteralPath $Script).Path
$dir = Split-Path -Parent ([System.IO.Path]::GetFullPath($Log))
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$Log = [System.IO.Path]::GetFullPath($Log)
$code = $Log + '.code'
$cmd = $Log + '.cmd'
$quoted = @($ScriptArgs | Where-Object { $_ -ne '--' } | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } })
$ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
@(
    '@echo off'
    ('"' + $ps + '" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $Script + '" ' + ($quoted -join ' ') + ' > "' + $Log + '" 2>&1 < NUL')
    ('echo %ERRORLEVEL% > "' + $code + '.tmp"')
    ('move /y "' + $code + '.tmp" "' + $code + '" > NUL')
) | Set-Content -Path $cmd -Encoding Ascii
Write-Output ('outside the job via WMI: ' + (Get-Content -Raw $cmd))
$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
    CommandLine = 'cmd.exe /c "' + $cmd + '"'; CurrentDirectory = $dir
}
if ($r.ReturnValue -ne 0) { throw ('Win32_Process.Create returned ' + $r.ReturnValue) }

# Stream new log bytes until the exit-code file appears.
$pos = 0L
function Write-NewLog {
    if (-not (Test-Path -LiteralPath $Log)) { return }
    $fs = [System.IO.File]::Open($Log, 'Open', 'Read', 'ReadWrite')
    try {
        if ($fs.Length -gt $script:pos) {
            $fs.Seek($script:pos, 'Begin') | Out-Null
            $buf = New-Object byte[] ($fs.Length - $script:pos)
            $n = $fs.Read($buf, 0, $buf.Length)
            $script:pos += $n
            [Console]::Out.Write([System.Text.Encoding]::UTF8.GetString($buf, 0, $n))
        }
    } finally { $fs.Dispose() }
}
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not (Test-Path -LiteralPath $code)) {
    if ($sw.Elapsed.TotalSeconds -gt $TimeoutSec) {
        Write-NewLog
        Write-Output ('::error::timed out after ' + $TimeoutSec + ' s')
        exit 124
    }
    Write-NewLog
    Start-Sleep -Seconds 2
}
Write-NewLog
$exit = [int](Get-Content -Raw -LiteralPath $code).Trim()
Write-Output ('script exit code: ' + $exit + ' (' + [int]$sw.Elapsed.TotalSeconds + ' s)')
exit $exit
