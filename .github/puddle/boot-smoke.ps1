# Boot smoke for a built msb.exe: create, exec, stop, start, exec, remove.
# Usage: boot-smoke.ps1 -Msb <msb.exe> -Libkrunfw <libkrunfw.dll>
# Uses its own MSB_HOME under RUNNER_TEMP. Fails on the first step that fails.
#
# Every msb call runs outside the Actions step's Job Object. `msb create`/`start` launch the
# VM runtime detached with CREATE_BREAKAWAY_FROM_JOB, and the runner's Job Object does not
# allow breakaway: called directly, `msb create` fails with "Access is denied. (os error 5)".
# The launcher (WMI Win32_Process.Create, else a Task Scheduler task) is picked by a preflight
# and named in the log.
param(
    [Parameter(Mandatory)] [string]$Msb,
    [Parameter(Mandatory)] [string]$Libkrunfw
)
# Continue: native stderr must not abort a step; failures are thrown explicitly.
$ErrorActionPreference = 'Continue'
$Msb = (Resolve-Path $Msb).Path
$Libkrunfw = (Resolve-Path $Libkrunfw).Path
$Work = Join-Path $env:RUNNER_TEMP 'puddle-smoke'
$MsbHome = Join-Path $Work 'home'
New-Item -ItemType Directory -Force -Path $MsbHome | Out-Null
$script:n = 0

function Invoke-Outside([string]$Launcher, [string]$ArgLine, [int]$TimeoutSec = 600) {
    $script:n++
    $id = '{0:D2}' -f $script:n
    $cmd = Join-Path $Work "step$id.cmd"
    $out = Join-Path $Work "step$id.out"
    $code = Join-Path $Work "step$id.code"
    @(
        '@echo off'
        "set `"MSB_HOME=$MsbHome`""
        "set `"MSB_PATH=$Msb`""
        "set `"MSB_LIBKRUNFW_PATH=$Libkrunfw`""
        "`"$Msb`" $ArgLine > `"$out`" 2>&1 < NUL"
        "echo %ERRORLEVEL% > `"$code.tmp`""
        "move /y `"$code.tmp`" `"$code`" > NUL"
    ) | Set-Content -Path $cmd -Encoding ascii
    $task = "puddle-smoke-$id"
    if ($Launcher -eq 'wmi') {
        $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
            CommandLine = "cmd.exe /c `"$cmd`""; CurrentDirectory = $Work
        }
        if ($r.ReturnValue -ne 0) { throw "Win32_Process.Create returned $($r.ReturnValue)" }
    } else {
        $action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument "/c `"$cmd`"" -WorkingDirectory $Work
        $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType S4U -RunLevel Highest
        Register-ScheduledTask -TaskName $task -Action $action -Principal $principal -Force | Out-Null
        Start-ScheduledTask -TaskName $task
    }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while (-not (Test-Path $code) -and $sw.Elapsed.TotalSeconds -lt $TimeoutSec) { Start-Sleep -Milliseconds 200 }
    if ($Launcher -eq 'task') { Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue }
    $text = if (Test-Path $out) { Get-Content $out -Raw } else { '' }
    if (-not (Test-Path $code)) { return [pscustomobject]@{ Code = -999; Text = "$text`n(timed out after $TimeoutSec s)"; Secs = $sw.Elapsed.TotalSeconds } }
    [pscustomobject]@{ Code = [int](Get-Content $code -Raw).Trim(); Text = "$text"; Secs = $sw.Elapsed.TotalSeconds }
}

$name = 'puddle-smoke'
function Step([string]$Label, [string]$ArgLine, [string]$Expect = '') {
    $r = Invoke-Outside $script:Launcher $ArgLine
    '{0}: exit {1} ({2:N1} s)' -f $Label, $r.Code, $r.Secs
    $r.Text.TrimEnd() -split "`r?`n" | ForEach-Object { "    $_" }
    if ($r.Code -ne 0) { throw "boot smoke step '$Label' exited $($r.Code)" }
    if ($Expect -and $r.Text -notmatch [regex]::Escape($Expect)) {
        throw "boot smoke step '$Label' did not print '$Expect'"
    }
}

# Preflight: a runner without WHP is an infrastructure problem, say so plainly.
$env:MSB_HOME = $MsbHome; $env:MSB_PATH = $Msb; $env:MSB_LIBKRUNFW_PATH = $Libkrunfw
$doctor = $null | & $Msb doctor 2>&1 | Out-String
$doctor
if ($doctor -notmatch 'Hypervisor\s+Windows Hypervisor Platform' -or $doctor -match '✗\s+Hypervisor') {
    throw 'WHP unavailable on this runner (infrastructure, not an msb failure)'
}

# Pick the launcher: the first one whose `msb create` works.
$script:Launcher = $null
foreach ($l in @('wmi', 'task')) {
    $r = Invoke-Outside $l "create alpine --name $name"
    'preflight create via {0}: exit {1} ({2:N1} s)' -f $l, $r.Code, $r.Secs
    $r.Text.TrimEnd() -split "`r?`n" | ForEach-Object { "    $_" }
    if ($r.Code -eq 0) { $script:Launcher = $l; break }
    $null = Invoke-Outside $l "rm -f $name"
}
if (-not $script:Launcher) { throw 'msb create failed with every launcher' }
"launcher: $script:Launcher (create above is the smoke's create step)"

Step 'version' '--version' 'puddle'
Step 'exec' "exec $name -- sh -c `"uname -r; cat /etc/alpine-release; echo smoke-ok`"" 'smoke-ok'
Step 'ls' 'ls' $name
Step 'stop' "stop $name"
Step 'start' "start $name"
Step 'exec2' "exec $name -- sh -c `"echo after-restart-ok`"" 'after-restart-ok'
Step 'rm' "rm -f $name"
Step 'ls2' 'ls'
$after = Invoke-Outside $script:Launcher 'ls'
if ($after.Text -match [regex]::Escape($name)) { throw "sandbox $name still listed after rm" }
'boot smoke: ok'
