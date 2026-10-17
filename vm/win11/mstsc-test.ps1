# Logon task of the test VM: if a data CD carries target.rdp (+ cred.txt = "user<TAB>password"),
# stores the credentials for the target host and opens the stock mstsc.exe on it.
# The script is fixed; the CD only provides data. A displays.txt ("WIDTHxHEIGHT") on the CD first puts every display
# of the guest at that size, side by side (manual tests with several monitors, see `win11.sh manual`).
$log = 'C:\oxydesk\mstsc-test.log'
# Same lines on COM1 (QEMU: -serial file) so that the host can read them.
try { $com = New-Object System.IO.Ports.SerialPort 'COM1', 115200; $com.Open() } catch { $com = $null }
function Log([string]$line) { "$(Get-Date -Format o) $line" | Out-File $log -Append; if ($com) { try { $com.WriteLine("MSTSC $line") } catch { } } }
function Set-Displays([int]$w, [int]$h, [bool]$left = $false) {
  Add-Type @'
using System; using System.Runtime.InteropServices;
public class Disp {
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)] public struct DEVMODE {
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
    public short dmSpecVersion, dmDriverVersion, dmSize, dmDriverExtra; public int dmFields;
    public int dmPositionX, dmPositionY, dmDisplayOrientation, dmDisplayFixedOutput;
    public short dmColor, dmDuplex, dmYResolution, dmTTOption, dmCollate;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
    public short dmLogPixels; public int dmBitsPerPel, dmPelsWidth, dmPelsHeight, dmDisplayFlags, dmDisplayFrequency;
    public int dmICMMethod, dmICMIntent, dmMediaType, dmDitherType, dmReserved1, dmReserved2, dmPanningWidth, dmPanningHeight;
  }
  [DllImport("user32.dll", CharSet = CharSet.Ansi)] public static extern bool EnumDisplaySettings(string dev, int mode, ref DEVMODE dm);
  [DllImport("user32.dll", CharSet = CharSet.Ansi)] public static extern int ChangeDisplaySettingsEx(string dev, ref DEVMODE dm, IntPtr hwnd, int flags, IntPtr lp);
  [DllImport("user32.dll", CharSet = CharSet.Ansi)] public static extern int ChangeDisplaySettingsEx(string dev, IntPtr dm, IntPtr hwnd, int flags, IntPtr lp);
}
'@
  # The attached displays are not necessarily DISPLAY1 and DISPLAY2 (the numbers of the basic adapter stay used), and
  # right after the logon the drivers may not be attached yet: wait (2 minutes at most) until some display answers.
  $devs = @()
  for ($t = 0; $t -lt 60 -and -not $devs; $t++) {
    foreach ($i in 1..8) {
      $dm = New-Object Disp+DEVMODE
      $dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf($dm)
      if ([Disp]::EnumDisplaySettings("\\.\DISPLAY$i", -1, [ref]$dm)) { $devs += "\\.\DISPLAY$i" }
    }
    if (-not $devs) { Start-Sleep -Seconds 2 }
  }
  Log "displays found: $($devs -join ' ')"
  $x = 0
  foreach ($dev in $devs) {
    $dm = New-Object Disp+DEVMODE
    $dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf($dm)
    [void][Disp]::EnumDisplaySettings($dev, -1, [ref]$dm)
    # DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT, kept in the registry (CDS_UPDATEREGISTRY), applied once all are set
    # (CDS_NORESET).
    $dm.dmFields = 0x20 -bor 0x80000 -bor 0x100000
    $dm.dmPelsWidth = $w; $dm.dmPelsHeight = $h; $dm.dmPositionX = $x; $dm.dmPositionY = 0
    $r = [Disp]::ChangeDisplaySettingsEx($dev, [ref]$dm, [IntPtr]::Zero, 0x1 -bor 0x10000000, [IntPtr]::Zero)
    Log "$dev ${w}x$h at $x -> $r"
    # "left": the other displays are at the left of the first one (negative coordinates, like a laptop with an external
    # screen on its left); otherwise they follow it to the right.
    if ($left) { $x -= $w } else { $x += $w }
  }
  $r = [Disp]::ChangeDisplaySettingsEx([NullString]::Value, [IntPtr]::Zero, [IntPtr]::Zero, 0, [IntPtr]::Zero)
  Log "apply -> $r"
  foreach ($dev in $devs) {
    $dm = New-Object Disp+DEVMODE
    $dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf($dm)
    [void][Disp]::EnumDisplaySettings($dev, -1, [ref]$dm)
    Log "$dev now $($dm.dmPelsWidth)x$($dm.dmPelsHeight) at $($dm.dmPositionX),$($dm.dmPositionY)"
  }
}
# Relative mouse (manual tests): no pointer acceleration and the default speed, so that a viewer's movement of N pixels
# moves the guest pointer by N pixels.
function Set-FlatMouse {
  Add-Type -Namespace Native -Name Mouse -MemberDefinition '[DllImport("user32.dll")] public static extern bool SystemParametersInfo(int a, int b, int[] c, int d); [DllImport("user32.dll")] public static extern bool SystemParametersInfo(int a, int b, IntPtr c, int d);'
  [void][Native.Mouse]::SystemParametersInfo(0x0004, 0, [int[]](0, 0, 0), 3)       # SPI_SETMOUSE: threshold 0/0, acceleration off
  [void][Native.Mouse]::SystemParametersInfo(0x0071, 0, [IntPtr]10, 3)              # SPI_SETMOUSESPEED: 10 = 1:1
  Log 'mouse: acceleration off'
}
foreach ($d in Get-PSDrive -PSProvider FileSystem) {
  $disp = Join-Path $d.Root 'displays.txt'
  if (Test-Path $disp) {
    $spec = ((Get-Content $disp -Raw).Trim() -split '\s+')
    $w, $h = ($spec[0] -split 'x', 2) | ForEach-Object { [int]$_ }
    $left = $spec -contains 'left'
    try { Set-FlatMouse } catch { Log "mouse: $_" }
    try { Set-Displays $w $h $left } catch { Log "displays: $_" }
    Start-Sleep -Seconds 3
    break
  }
}
# An rdpaccess.txt on the CD (`win11.sh manual`): the guest itself is made reachable by Remote Desktop (elevated through
# the UAC prompt, which the host accepts) and mstsc is not started: it is opened from the desktop icon, in the session of
# the human tester's own RDP client (a single window with the tester's monitors, absolute pointer).
$remote = $false
foreach ($d in Get-PSDrive -PSProvider FileSystem) { if (Test-Path (Join-Path $d.Root 'rdpaccess.txt')) { $remote = $true } }
if ($remote) {
  $enable = 'reg add "HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server" /v fDenyTSConnections /t REG_DWORD /d 0 /f; ' +
            'Set-Service TermService -StartupType Automatic; Start-Service TermService; ' +
            'Enable-NetFirewallRule -DisplayGroup "Remote Desktop"; ' +
            '$c = New-Object System.IO.Ports.SerialPort "COM1", 115200; $c.Open(); $c.WriteLine("MSTSC rdp enabled"); $c.Close()'
  $com.Close(); $com = $null
  # Encoded: the quotes of the command would not survive the command line of the elevated process.
  $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($enable))
  Start-Process powershell.exe -Verb RunAs -Wait -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded
  try { $com = New-Object System.IO.Ports.SerialPort 'COM1', 115200; $com.Open() } catch { $com = $null }
}
foreach ($d in Get-PSDrive -PSProvider FileSystem) {
  $rdp = Join-Path $d.Root 'target.rdp'
  $cred = Join-Path $d.Root 'cred.txt'
  if ((Test-Path $rdp) -and (Test-Path $cred)) {
    $user, $pass = (Get-Content $cred -Raw).Trim() -split "`t", 2
    $addr = ((Get-Content $rdp | Where-Object { $_ -like 'full address:s:*' }) -split ':s:', 2)[1]
    $host_ = $addr.Split(':')[0]
    cmdkey /generic:"TERMSRV/$host_" /user:$user /pass:$pass | Out-Null
    Copy-Item $rdp C:\oxydesk\target.rdp -Force
    "$(Get-Date -Format o) mstsc -> $addr" | Out-File $log -Append
    # cursorlog.txt holding "nomstsc": pointer test of the bare guest.
    $skip = $remote
    if ($remote) { Copy-Item $rdp (Join-Path ([Environment]::GetFolderPath('Desktop')) 'oxydesk.rdp') -Force }
    foreach ($c in Get-PSDrive -PSProvider FileSystem) { $f = Join-Path $c.Root 'cursorlog.txt'; if ((Test-Path $f) -and ((Get-Content $f -Raw) -match 'nomstsc')) { $skip = $true } }
    if (-not $skip) { Start-Process mstsc.exe -ArgumentList 'C:\oxydesk\target.rdp' }
    break
  }
}
# A cursorlog.txt on the CD (WIN_CURSORLOG=1 with `win11.sh manual`): the pointer position is sent to COM1 whenever it
# changes, to find out where the guest puts the pointer fed by each VNC display.
foreach ($d in Get-PSDrive -PSProvider FileSystem) {
  if (Test-Path (Join-Path $d.Root 'cursorlog.txt')) {
    Add-Type -AssemblyName System.Windows.Forms
    $last = ''
    while ($true) {
      $p = [System.Windows.Forms.Cursor]::Position
      $now = "$($p.X),$($p.Y)"
      if ($now -ne $last) { Log "cursor $now"; $last = $now }
      Start-Sleep -Milliseconds 100
    }
  }
}
# Remote Desktop access (see above): the logon session is the console one, which the tester's RDP client takes over, and
# Windows only starts rdpclip.exe (the clipboard relay) when an RDP session is opened. Keep one running: it quits
# until a client's clipboard channel exists, then stays.
if ($remote) {
  $was = $null
  # Also reported (when they change): the monitors Windows has and where the mstsc window is.
  Add-Type -AssemblyName System.Windows.Forms
  Add-Type -Namespace Native -Name Win -MemberDefinition '[StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; } [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);'
  $lastView = ''
  while ($true) {
    $screens = ([System.Windows.Forms.Screen]::AllScreens | ForEach-Object { "$($_.Bounds.X),$($_.Bounds.Y) $($_.Bounds.Width)x$($_.Bounds.Height)" }) -join ' | '
    $win = ''
    foreach ($m in Get-Process mstsc -ErrorAction SilentlyContinue) {
      $r = New-Object Native.Win+RECT
      if ($m.MainWindowHandle -ne 0 -and [Native.Win]::GetWindowRect($m.MainWindowHandle, [ref]$r)) { $win += " mstsc[$($m.Id)] $($r.L),$($r.T) $($r.R - $r.L)x$($r.B - $r.T)" }
    }
    if ("$screens$win" -ne $lastView) { Log "screens: $screens;$win"; $lastView = "$screens$win" }
    $running = [bool](Get-Process rdpclip -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq (Get-Process -Id $PID).SessionId })
    if (-not $running) { Start-Process rdpclip.exe -ErrorAction SilentlyContinue }
    if ($running -ne $was) { Log "rdpclip running: $running"; $was = $running }
    Start-Sleep -Seconds 3
  }
}
