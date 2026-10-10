# One-shot, run at logon from the data CD (as mstsc-test.ps1): installs the display drivers of the
# virtio-win CD (QXL-WDDM and virtio-gpu) so the test VM can have a multi-head display adapter.
$ErrorActionPreference = 'Continue'
$com = $null
try { $com = New-Object System.IO.Ports.SerialPort 'COM1', 115200; $com.Open() } catch { }
function Emit([string]$l) { if ($com -and $com.IsOpen) { $com.WriteLine($l) } }
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Emit "DRIVERS admin=$admin"
if (-not $admin) {
  # Elevation through the UAC consent prompt (the host accepts it with a key press); the child owns COM1.
  $self = $MyInvocation.MyCommand.Path
  $com.Close()
  Start-Process powershell.exe -Verb RunAs -Wait -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$self`""
  exit 0
}
$vio = [System.IO.DriveInfo]::GetDrives() | Where-Object { $_.DriveType -eq 'CDRom' -and $_.IsReady -and (Test-Path (Join-Path $_.RootDirectory.FullName 'qxldod')) } | Select-Object -First 1
if (-not $vio) { Emit 'DRIVERS no virtio-win CD'; Start-Sleep 2; Stop-Computer -Force; exit 1 }
$root = $vio.RootDirectory.FullName
foreach ($d in 'qxldod', 'viogpudo') {
  foreach ($os in 'w11', 'w10') {
    $dir = Join-Path $root "$d\$os\amd64"
    if (Test-Path $dir) {
      $out = & pnputil.exe /add-driver "$dir\*.inf" /install 2>&1 | Out-String
      Emit ("DRIVERS $d/$os " + $(if ($out -match 'Added driver packages:\s+1') { 'ADDED ' } else { 'FAILED ' }) + $out.Replace("`r", '').Replace("`n", ' | '))
      break
    }
  }
}
Emit 'DRIVERS done'
Start-Sleep 3
Stop-Computer -Force
