# One-shot provisioning of the disposable Windows test VM, run at the first logon from the answer CD.
# Fixed content only: it installs mstsc-test.ps1 as a logon task; the CD otherwise provides data.
$ErrorActionPreference = 'Continue'
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0
reg add 'HKLM\SOFTWARE\Policies\Microsoft\Windows\Personalization' /v NoLockScreen /t REG_DWORD /d 1 /f | Out-Null
New-Item -ItemType Directory -Force C:\oxydesk | Out-Null
# mstsc.exe defaults for the tester: no consent dialog for .rdp files, clipboard redirection on.
reg add 'HKCU\Software\Microsoft\Terminal Server Client' /v RdpLaunchConsentAccepted /t REG_DWORD /d 1 /f | Out-Null
reg add 'HKCU\Software\Microsoft\Terminal Server Client' /v 'Redirect Clipboard' /t REG_DWORD /d 1 /f | Out-Null
# Stub task: runs, from the data CD, the versioned test script (so it can change without reinstalling).
Set-Content C:\oxydesk\run-from-cd.ps1 @'
foreach ($d in Get-PSDrive -PSProvider FileSystem) {
  foreach ($name in 'rdp-activex-test.ps1', 'mstsc-test.ps1') {
    $f = Join-Path $d.Root $name
    if (Test-Path $f) { & $f }
  }
}
'@
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\oxydesk\run-from-cd.ps1'
$trigger = New-ScheduledTaskTrigger -AtLogOn -User 'tester'
Register-ScheduledTask -TaskName 'oxydesk-tests' -Action $action -Trigger $trigger -User 'tester' -Force | Out-Null
'provisioned' | Out-File C:\oxydesk\first-logon.done
try { $c = New-Object System.IO.Ports.SerialPort 'COM1', 115200; $c.Open(); $c.WriteLine('PROVISIONED'); $c.Close() } catch { }
