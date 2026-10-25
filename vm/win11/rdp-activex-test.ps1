# Deterministic RDP test run in the Windows test VM, no human or LLM in the loop.
# Hosts Microsoft's RDP ActiveX control (mstscax.dll, the engine of mstsc.exe) in a WinForms window,
# connects to the target given by params.txt on the data CD and reports one JSON line per check on
# COM1 (QEMU: -serial file). Fixed script: the CD only provides data.
#   params.txt: key=value lines (server, port, user, password, width, height)
# The script first prints READY and waits for a params.txt (any drive); the host snapshots the VM at
# that point (savevm) and, for each test, restores it, swaps the data CD and the run starts at once.
$ErrorActionPreference = 'Stop'
trap { try { Emit ('ERROR ' + ($_ | Out-String).Replace("`r", '').Replace("`n", ' | ')) } catch { }; try { Start-Sleep 2; Stop-Computer -Force } catch { }; exit 1 }
# Images installed before the rename from C:\rdsktp do not have this folder.
New-Item -ItemType Directory -Force C:\oxydesk | Out-Null
$log = 'C:\oxydesk\activex-test.log'
$checks = New-Object System.Collections.ArrayList
$com = $null
try { $com = New-Object System.IO.Ports.SerialPort 'COM1', 115200; $com.Open() } catch { }
function Emit([string]$line) {
  $line | Out-File $log -Append -Encoding ascii
  if ($com -and $com.IsOpen) { $com.WriteLine($line) }
}
function Check([string]$name, [bool]$ok, [string]$detail) {
  [void]$checks.Add(@{ name = $name; passed = $ok; detail = $detail })
  Emit ("CHECK " + (@{ name = $name; passed = $ok; detail = $detail } | ConvertTo-Json -Compress))
}
function Finish {
  $failed = @($checks | Where-Object { -not $_.passed }).Count
  Emit ("RESULT " + (@{ passed = ($failed -eq 0 -and $checks.Count -gt 0); checks = $checks.Count; failed = $failed } | ConvertTo-Json -Compress))
  Emit 'END'
  Start-Sleep -Seconds 2
  Stop-Computer -Force
  exit 0
}

# --- parameters from the data CD -------------------------------------------------------------
function FindParams {
  # DriveInfo (not Get-PSDrive): sees media inserted after the snapshot was taken.
  foreach ($d in [System.IO.DriveInfo]::GetDrives()) {
    if ($d.DriveType -eq 'CDRom' -and $d.IsReady) {
      $f = Join-Path $d.RootDirectory.FullName 'params.txt'
      if ([System.IO.File]::Exists($f)) { return $f }
    }
  }
  return $null
}
Emit 'READY'
$pf = $null
$n = 0
while (-not ($pf = FindParams)) {
  Start-Sleep -Milliseconds 300
  if (++$n % 20 -eq 0) {
    Emit ('WAIT ' + (([System.IO.DriveInfo]::GetDrives() | Where-Object { $_.DriveType -eq 'CDRom' } |
      ForEach-Object { "$($_.Name)ready=$($_.IsReady)" + $(if ($_.IsReady) { ":$($_.VolumeLabel)" }) }) -join ' '))
  }
}
Start-Sleep -Milliseconds 500   # let the CD content settle
$p = @{}
foreach ($l in Get-Content $pf) { if ($l -match '^([^=#]+)=(.*)$') { $p[$Matches[1].Trim()] = $Matches[2].Trim() } }
Add-Type @'
using System; using System.Runtime.InteropServices;
public class DispLeft {
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
# The displays as they are now, from the driver (System.Windows.Forms.Screen keeps the layout it saw first when nothing
# pumps messages): [pscustomobject] with DeviceName and Bounds (X, Y, Width, Height), left to right.
function Get-Screens {
  $list = @()
  foreach ($i in 1..8) {
    $dev = "\\.\DISPLAY$i"
    $dm = New-Object DispLeft+DEVMODE
    $dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf($dm)
    if (-not [DispLeft]::EnumDisplaySettings($dev, -1, [ref]$dm) -or $dm.dmPelsWidth -eq 0) { continue }
    $list += [pscustomobject]@{ DeviceName = $dev; Bounds = [pscustomobject]@{ X = $dm.dmPositionX; Y = $dm.dmPositionY; Width = $dm.dmPelsWidth; Height = $dm.dmPelsHeight } }
  }
  return @($list | Sort-Object { $_.Bounds.X })
}
# Puts every display at 1920x1080 (the size of the outputs of the test session, which needs no scaling) with the first one
# at (0,0), as the primary one, and the others at its left (negative coordinates): a laptop with an external screen on its
# left.
function Move-DisplaysLeft {
  $x = 0
  foreach ($i in 1..8) {
    $dev = "\\.\DISPLAY$i"
    $dm = New-Object DispLeft+DEVMODE
    $dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf($dm)
    if (-not [DispLeft]::EnumDisplaySettings($dev, -1, [ref]$dm) -or $dm.dmPelsWidth -eq 0) { continue }
    # DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT, kept in the registry (CDS_UPDATEREGISTRY) and applied at the end (CDS_NORESET).
    $width = 1920
    $dm.dmFields = 0x20 -bor 0x80000 -bor 0x100000
    $dm.dmPelsWidth = 1920; $dm.dmPelsHeight = 1080
    $dm.dmPositionX = $x; $dm.dmPositionY = 0
    # The mode must be one of the driver's: take its own 1920x1080 entry (colour depth and frequency included).
    foreach ($m in 0..400) {
      $c = New-Object DispLeft+DEVMODE; $c.dmSize = $dm.dmSize
      if (-not [DispLeft]::EnumDisplaySettings($dev, $m, [ref]$c)) { break }
      if ($c.dmPelsWidth -eq 1920 -and $c.dmPelsHeight -eq 1080) { $dm.dmBitsPerPel = $c.dmBitsPerPel; $dm.dmDisplayFrequency = $c.dmDisplayFrequency; $dm.dmFields = $dm.dmFields -bor 0x40000 -bor 0x400000; break }
    }
    $r = [DispLeft]::ChangeDisplaySettingsEx($dev, [ref]$dm, [IntPtr]::Zero, 0x1 -bor 0x10000000, [IntPtr]::Zero)
    Emit "DISPLAYLEFT $dev $width at $x -> $r"
    $x -= $width
  }
  [void][DispLeft]::ChangeDisplaySettingsEx([NullString]::Value, [IntPtr]::Zero, [IntPtr]::Zero, 0, [IntPtr]::Zero)
  Start-Sleep -Seconds 3
}
Emit "BEGIN $(Get-Date -Format o) server=$($p.server):$($p.port)"
$w = [int]$p.width; $h = [int]$p.height
if (-not $p.big_mib) { $p.big_mib = '128' }

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Emit ('SCREENS ' + (([System.Windows.Forms.Screen]::AllScreens | ForEach-Object { "$($_.DeviceName)=$($_.Bounds)" }) -join ' '))
if ($p.monitors -eq '2') {
  # Multi-monitor run: the stock mstsc.exe in full screen over both local monitors (the ActiveX control cannot
  # do full screen multimon without a container). Observed through screenshots of the whole virtual screen.
  if ($p.left -eq '1') { Move-DisplaysLeft }
  $scr = Get-Screens
  $x0 = ($scr | ForEach-Object { $_.Bounds.X } | Measure-Object -Minimum).Minimum
  $y0 = ($scr | ForEach-Object { $_.Bounds.Y } | Measure-Object -Minimum).Minimum
  $x1 = ($scr | ForEach-Object { $_.Bounds.X + $_.Bounds.Width } | Measure-Object -Maximum).Maximum
  $y1 = ($scr | ForEach-Object { $_.Bounds.Y + $_.Bounds.Height } | Measure-Object -Maximum).Maximum
  $script:vb = New-Object System.Drawing.Rectangle $x0, $y0, ($x1 - $x0), ($y1 - $y0)
  Emit ('MONITORS ' + (($scr | ForEach-Object { "$($_.DeviceName)=$($_.Bounds.X),$($_.Bounds.Y) $($_.Bounds.Width)x$($_.Bounds.Height)" }) -join ' '))
  function ScreenBmp {
    $bmp = New-Object System.Drawing.Bitmap $script:vb.Width, $script:vb.Height
    $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($script:vb.Location, [System.Drawing.Point]::Empty, $script:vb.Size); $g.Dispose()
    return $bmp
  }
  function ShotMulti([string]$tag) {
    $bmp = ScreenBmp
    $small = New-Object System.Drawing.Bitmap ([int]($bmp.Width / 4)), ([int]($bmp.Height / 4))
    $gs = [System.Drawing.Graphics]::FromImage($small); $gs.DrawImage($bmp, 0, 0, $small.Width, $small.Height); $gs.Dispose()
    $pm = New-Object System.IO.MemoryStream; $small.Save($pm, [System.Drawing.Imaging.ImageFormat]::Png); $small.Dispose(); $bmp.Dispose()
    Emit ("SHOT $tag " + [Convert]::ToBase64String($pm.ToArray()))
  }
  cmdkey /generic:"TERMSRV/$($p.server)" /user:$($p.user) /pass:$($p.password) | Out-Null
  # Default.rdp (base of every command-line launch): no "unknown publisher" consent dialog as for a .rdp argument.
  @('screen mode id:i:2', 'use multimon:i:1', 'authentication level:i:0', 'prompt for credentials:i:0',
    'redirectclipboard:i:0', 'audiomode:i:2', "username:s:$($p.user)", 'dynamic resolution:i:1') |
    Set-Content (Join-Path ([Environment]::GetFolderPath('MyDocuments')) 'Default.rdp') -Encoding ascii -Force
  Attrib -S -H (Join-Path ([Environment]::GetFolderPath('MyDocuments')) 'Default.rdp') 2>$null
  $mstsc = Start-Process mstsc.exe -ArgumentList "/v:$($p.server):$($p.port)", '/f', '/multimon' -PassThru
  Start-Sleep -Seconds 12
  ShotMulti 't12'
  Start-Sleep -Seconds 4
  ShotMulti 't16'
  # Each local monitor must show its own remote output: the synthetic desktop draws a heartbeat block at (32,0) of every
  # output and, after the first, an identity block at (64,0) with PALETTE[rank] (second output: green).
  $bmp = ScreenBmp
  function Near($c, $r, $g, $b) { [math]::Abs($c.R - $r) -lt 70 -and [math]::Abs($c.G - $g) -lt 70 -and [math]::Abs($c.B - $b) -lt 70 }  # H.264 shifts colours
  $ids = @(foreach ($sc in $scr) { $c = $bmp.GetPixel($sc.Bounds.X - $script:vb.X + 80, $sc.Bounds.Y - $script:vb.Y + 16); "$($c.R),$($c.G),$($c.B)" })
  $bmp.Dispose()
  Check 'multi_connect' (-not $mstsc.HasExited) "mstsc running=$(-not $mstsc.HasExited)"
  # The first output (red identity: it was the leftmost of the session at its start) is the client's primary monitor, the
  # one at (0,0); the second output (green) is the other one, wherever it is.
  $iP = [array]::IndexOf(@($scr | ForEach-Object { $_.Bounds.X -eq 0 -and $_.Bounds.Y -eq 0 }), $true)
  $iO = 1 - $iP
  function IdIs($i, $r, $g, $b) { $i -ge 0 -and $ids.Count -gt $i -and ($ids[$i] -match '^(\d+),(\d+),(\d+)$') -and (Near ([pscustomobject]@{ R = [int]$Matches[1]; G = [int]$Matches[2]; B = [int]$Matches[3] }) $r $g $b) }
  Check 'multi_first_output_on_primary_monitor' (IdIs $iP 255 0 0) "identity pixel (80,16) of each monitor, left to right: $($ids -join ' | '); the primary monitor is number $($iP + 1) (first output expected red 255,0,0 there)"
  Check 'multi_second_output_on_other_monitor' (IdIs $iO 0 255 0) "identity pixel (80,16) of each monitor, left to right: $($ids -join ' | '); the other monitor is number $($iO + 1) (second output expected green 0,255,0 there)"

  # The server was started with other sizes than the guest monitors: it must have adopted the monitors announced by
  # the client at connection (each output ends with a magenta block in its bottom-right corner).
  $bmp = ScreenBmp
  $corners = @(foreach ($sc in $scr) {
    $c = $bmp.GetPixel($sc.Bounds.X - $script:vb.X + $sc.Bounds.Width - 16, $sc.Bounds.Y - $script:vb.Y + $sc.Bounds.Height - 16)
    [pscustomobject]@{ R = $c.R; G = $c.G; B = $c.B; Sz = "$($sc.Bounds.Width)x$($sc.Bounds.Height)" }
  })
  $bmp.Dispose()
  $allMagenta = ($corners.Count -ge 2) -and -not ($corners | Where-Object { -not ($_.R -gt 180 -and $_.G -lt 90 -and $_.B -gt 180) })
  Check 'multi_layout_follows_client_monitors' ([bool]$allMagenta) "corner pixel of each monitor: $(($corners | ForEach-Object { "$($_.Sz) $($_.R),$($_.G),$($_.B)" }) -join ' | ') (magenta expected)"
  # Pointer: a click at a given place of a monitor must land on the output shown by that monitor, at the same place (the
  # test desktop draws a cyan block where a button is pressed).
  Add-Type -Namespace Native -Name Click -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y); [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);'
  function CyanNear($bmp, [int]$cx, [int]$cy) {
    for ($dy = -24; $dy -le 24; $dy += 4) { for ($dx = -24; $dx -le 24; $dx += 4) {
      $x = $cx + $dx; $y = $cy + $dy
      if ($x -ge 0 -and $y -ge 0 -and $x -lt $bmp.Width -and $y -lt $bmp.Height) {
        $c = $bmp.GetPixel($x, $y); if ($c.R -lt 110 -and $c.G -gt 170 -and $c.B -gt 170) { return $true }
      } } }
    return $false
  }
  # Six places (corners and middle) taken one after the other: the first three on the leftmost monitor, the others on the
  # next one. Every place is used once, so that the mark of a click cannot be taken for another one's; the same place of
  # the other monitor must stay free (a click that lands on the other monitor leaves its mark there).
  $spots = @(@(100, 100), @(1800, 150), @(150, 950), @(1700, 950), @(960, 540), @(500, 300))
  $n = 0
  foreach ($sc in $scr) {
    foreach ($k in 1..3) {
      $spot = $spots[$n]
      $gx = $sc.Bounds.X + $spot[0]; $gy = $sc.Bounds.Y + $spot[1]
      # The first move into a monitor can be reported with the last position seen before: settle with a second one.
      [void][Native.Click]::SetCursorPos($gx, $gy); Start-Sleep -Milliseconds 400
      [void][Native.Click]::SetCursorPos($gx + 1, $gy); Start-Sleep -Milliseconds 200
      [void][Native.Click]::SetCursorPos($gx, $gy); Start-Sleep -Milliseconds 400
      [Native.Click]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 100
      [Native.Click]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Seconds 2
      $bmp = ScreenBmp
      $onSame = CyanNear $bmp ($gx - $script:vb.X) ($gy - $script:vb.Y)
      $others = @(foreach ($o in $scr) { if ($o.DeviceName -ne $sc.DeviceName) { $o.DeviceName + '=' + (CyanNear $bmp ($o.Bounds.X - $script:vb.X + $spot[0]) ($o.Bounds.Y - $script:vb.Y + $spot[1])) } })
      $bmp.Dispose()
      $n++
      Check "multi_click_$n" ([bool]($onSame -and ($others -notmatch '=True'))) "click at $gx,$gy (monitor $($sc.DeviceName) at $($sc.Bounds.X),$($sc.Bounds.Y), place $($spot[0]),$($spot[1])): mark at the same place of that monitor=$onSame; same place of the others: $($others -join ' ')"
    }
  }
  ShotMulti 'clicks'
  try { Stop-Process -Id $mstsc.Id -Force } catch { }
  Finish
}
Emit ('SCREENS ' + (([System.Windows.Forms.Screen]::AllScreens | ForEach-Object { "$($_.DeviceName)=$($_.Bounds)" }) -join ' ') +
  ' ADAPTERS ' + ((Get-CimInstance Win32_VideoController | ForEach-Object { "$($_.Name):$($_.Status)" }) -join ' ; '))
# Newest "NotSafeForScripting" flavour of the control (it exposes ClearTextPassword).
$type = $null
foreach ($v in 12..6) {
  $type = [Type]::GetTypeFromProgID("MsRdpClient${v}NotSafeForScripting")
  if ($type) { Emit "control MsRdpClient${v}NotSafeForScripting"; break }
}
if (-not $type) { $type = [Type]::GetTypeFromProgID('MsTscAx.MsTscAx'); Emit 'control MsTscAx.MsTscAx' }
if (-not $type) { throw 'no RDP ActiveX control registered' }
$clsid = $type.GUID.ToString('B')
Add-Type -ReferencedAssemblies System.Windows.Forms -TypeDefinition @"
public class Win32Clip {
  [System.Runtime.InteropServices.DllImport("user32.dll")]
  public static extern uint GetClipboardSequenceNumber();
  [System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
  public static extern System.IntPtr LoadKeyboardLayout(string klid, uint flags);
  [System.Runtime.InteropServices.DllImport("user32.dll")]
  public static extern System.IntPtr ActivateKeyboardLayout(System.IntPtr hkl, uint flags);
  [System.Runtime.InteropServices.DllImport("user32.dll")]
  public static extern System.IntPtr GetKeyboardLayout(uint thread);
}
[System.Runtime.InteropServices.ComImport,
 System.Runtime.InteropServices.Guid("c1e6743a-41c1-4a74-832a-0dd06c1c7a0e"),
 System.Runtime.InteropServices.InterfaceType(System.Runtime.InteropServices.ComInterfaceType.InterfaceIsIUnknown)]
interface IMsTscNonScriptable {
  [System.Runtime.InteropServices.PreserveSig]
  int put_ClearTextPassword([System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.BStr)] string p);
}
[System.Runtime.InteropServices.ComImport,
 System.Runtime.InteropServices.Guid("302D8188-0052-4807-806A-362B628F9AC5"),
 System.Runtime.InteropServices.InterfaceType(System.Runtime.InteropServices.ComInterfaceType.InterfaceIsIUnknown)]
interface IMsRdpExtendedSettings {
  void set_Property([System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.BStr)] string name, [System.Runtime.InteropServices.In] ref object value);
  object get_Property([System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.BStr)] string name);
}
public class RdpHost : System.Windows.Forms.AxHost {
  public void SetExt(string name, object v) { ((IMsRdpExtendedSettings)GetOcx()).set_Property(name, ref v); }
  public object GetExt(string name) { return ((IMsRdpExtendedSettings)GetOcx()).get_Property(name); }
  public RdpHost(string clsid) : base(clsid) {}
  public object Ocx { get { return GetOcx(); } }
  public int SetPassword(string p) { return ((IMsTscNonScriptable)GetOcx()).put_ClearTextPassword(p); }
}
"@

Add-Type -TypeDefinition @'
public static class DrivePattern {
  static byte At(long i) { return (byte)((i * 31 + 7) % 251); }
  public static void Write(string path, long size) {
    using (var f = System.IO.File.Create(path)) {
      var b = new byte[65536]; long done = 0;
      while (done < size) { int n = (int)System.Math.Min(b.Length, size - done); for (int i = 0; i < n; i++) b[i] = At(done + i); f.Write(b, 0, n); done += n; }
    }
  }
  // null if the file holds exactly the pattern of `size` bytes, else the first difference.
  public static string Verify(string path, long size) {
    using (var f = System.IO.File.OpenRead(path)) {
      if (f.Length != size) return "size " + f.Length + " instead of " + size;
      var b = new byte[65536]; long done = 0;
      while (done < size) {
        int n = f.Read(b, 0, (int)System.Math.Min(b.Length, size - done));
        if (n <= 0) return "short read at " + done;
        for (int i = 0; i < n; i++) if (b[i] != At(done + i)) return "difference at byte " + (done + i);
        done += n;
      }
    }
    return null;
  }
}
'@
Emit 'STEP types compiled'
# Reference tone played locally (not through RDP): shows that the guest/QEMU audio chain records a known 1000 Hz tone (the RDP tone is 440 Hz).
$rate = 48000; $n = $rate * 2
$pcm = New-Object byte[] ($n * 4)
for ($i = 0; $i -lt $n; $i++) {
  $v = [int16](16000 * [math]::Sin(2 * [math]::PI * 1000 * $i / $rate))
  $b = [BitConverter]::GetBytes($v)
  $pcm[$i * 4] = $b[0]; $pcm[$i * 4 + 1] = $b[1]; $pcm[$i * 4 + 2] = $b[0]; $pcm[$i * 4 + 3] = $b[1]
}
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $ms
$bw.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $bw.Write([int](36 + $pcm.Length)); $bw.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
$bw.Write([int]16); $bw.Write([int16]1); $bw.Write([int16]2); $bw.Write([int]$rate); $bw.Write([int]($rate * 4)); $bw.Write([int16]4); $bw.Write([int16]16)
$bw.Write([Text.Encoding]::ASCII.GetBytes('data')); $bw.Write([int]$pcm.Length); $bw.Write($pcm); $bw.Flush(); $ms.Position = 0
(New-Object System.Media.SoundPlayer $ms).PlaySync()
Emit 'STEP reference tone played'
$form = New-Object System.Windows.Forms.Form
$form.StartPosition = 'Manual'; $form.Location = '0,0'; $form.FormBorderStyle = 'None'
$form.ClientSize = New-Object System.Drawing.Size($w, $h); $form.TopMost = $true
$host_ = New-Object RdpHost $clsid
$host_.Dock = 'Fill'
$form.Controls.Add($host_)
$form.Show()
$ocx = $host_.Ocx
Emit 'STEP control created'

$ocx.Server = $p.server
$ocx.UserName = $p.user
$ocx.DesktopWidth = $w; $ocx.DesktopHeight = $h
$adv = $ocx.AdvancedSettings9
$adv.RDPPort = [int]$p.port
$adv.EnableCredSspSupport = $true       # default mstsc behaviour: offers CredSSP, falls back to TLS
$adv.AuthenticationLevel = 0            # no server certificate warning (self-signed)
$adv.RedirectClipboard = $true
$drivesDir = 'C:\oxydesk\drive'
if ($p.drives -eq '1') {
  # Drive redirection: the drives of this machine are mounted by the server. The files the host checks are prepared
  # here, before the connection (the host reads them through the mount and writes others).
  $adv.RedirectDrives = $true
  Remove-Item $drivesDir -Recurse -Force -ErrorAction SilentlyContinue
  New-Item -ItemType Directory $drivesDir | Out-Null
  [DrivePattern]::Write("$drivesDir\guest.bin", 3MB + 5)
  [IO.File]::WriteAllText("$drivesDir\guest.txt", 'to delete', [Text.Encoding]::UTF8)
  [IO.File]::WriteAllText("$drivesDir\to-rename.bin", 'rename me', [Text.Encoding]::UTF8)
}
if ($p.printers -in @('1','2')) {
  # printers=2: XPS jobs (ADR 015): the printer OxydeskXps has the "Microsoft PS Class Driver", the server is told to send it XPS
  # (--printer-mode OxydeskXps=xps) and the driver renders the XPS document it gets in PostScript. printers=1: raw jobs.
  $drv = if ($p.printers -eq '2') { 'Microsoft PS Class Driver' } else { 'Generic / Text Only' }
  $pname = if ($p.printers -eq '2') { 'OxydeskXps' } else { 'OxydeskTest' }
  # Printer redirection: a "Generic / Text Only" printer on a TCP/IP port (local port 9100) where this script listens.
  # The host sends a job to the socket the server makes for the printer; the spooler of this machine sends it to the
  # listener, checked below. (A printer on a file port is not redirected by mstsc.)
  $adv.RedirectPrinters = $true
  try {
    Get-Printer -Name 'OxydeskTest','OxydeskXps' -ErrorAction SilentlyContinue | Remove-Printer
    Get-PrinterPort -Name 'OxydeskPort' -ErrorAction SilentlyContinue | Remove-PrinterPort -ErrorAction SilentlyContinue
    Add-PrinterPort -Name 'OxydeskPort' -PrinterHostAddress '127.0.0.1' -PortNumber 9100 -ErrorAction Stop
    if (-not (Get-PrinterDriver -Name $drv -ErrorAction SilentlyContinue)) { Add-PrinterDriver -Name $drv -ErrorAction Stop }
    Add-Printer -Name $pname -DriverName $drv -PortName 'OxydeskPort' -ErrorAction Stop
  } catch { Emit ("PRINTER SETUP FAILED: " + $_) }
  Emit ('DRIVERS ' + ((Get-PrinterDriver -ErrorAction SilentlyContinue | ForEach-Object { $_.Name }) -join ', '))
  Emit ('PRINTERS ' + ((Get-Printer | ForEach-Object { $_.Name + '@' + $_.PortName + '/' + $_.PrinterStatus }) -join ', '))
  $listener = New-Object System.Net.Sockets.TcpListener ([System.Net.IPAddress]::Parse('127.0.0.1')), 9100
  $listener.Start()
  $accepting = $listener.AcceptTcpClientAsync()
}
$adv.AudioRedirectionMode = 0   # play on this computer
$adv.SmartSizing = $false
# ClearTextPassword lives on the non-scriptable interface (declared above).
$hr = $host_.SetPassword($p.password)
if ($hr -ne 0) { throw "put_ClearTextPassword failed: 0x$('{0:X8}' -f $hr)" }

# No COM events from PowerShell: poll Connected (0 = disconnected, 1 = connected, 2 = connecting).
$script:everConnected = $false
$state = New-Object psobject
$state | Add-Member ScriptProperty connected { $c = ($ocx.Connected -eq 1); if ($c) { $script:everConnected = $true }; $c }
$state | Add-Member ScriptProperty disconnected { ($ocx.Connected -eq 0) -and ($script:everConnected -or $script:sawConnecting) }
$state | Add-Member ScriptProperty reason { try { $ocx.ExtendedDisconnectReason } catch { -1 } }
function Pump([scriptblock]$until, [int]$ms) {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ($sw.ElapsedMilliseconds -lt $ms) {
    [System.Windows.Forms.Application]::DoEvents()
    if ($ocx.Connected -eq 2) { $script:sawConnecting = $true }
    if (& $until) { return $true }
    Start-Sleep -Milliseconds 20
  }
  return $false
}
function PixelAt([int]$x, [int]$y) {
  $bmp = New-Object System.Drawing.Bitmap 1, 1
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($form.Location.X + $x, $form.Location.Y + $y, 0, 0, (New-Object System.Drawing.Size(1, 1)))
  $c = $bmp.GetPixel(0, 0); $g.Dispose(); $bmp.Dispose()
  return $c
}

# Keyboard layout announced by mstsc at connection: the one of the thread that hosts the control.
if ($p.klid) {
  $hkl = [Win32Clip]::LoadKeyboardLayout(('0000' + $p.klid), 1)
  $null = [Win32Clip]::ActivateKeyboardLayout($hkl, 0)
  Emit ('STEP keyboard layout of the thread: 0x{0:X8}' -f [Win32Clip]::GetKeyboardLayout(0).ToInt64())
}
Emit 'STEP connecting'
try {
  $ocx.Connect(); $script:connectCalled = $true
  $ok = Pump { $state.connected -or $state.disconnected } 30000
  Check 'connect' ($ok -and $state.connected) ("connected=$($state.connected) disconnectReason=$($state.reason)")
  if (-not $state.connected) { Finish }

  if ($p.remote_files -eq '1') {
    # Files copied in the remote session by a file manager (the host runs the copy once it reads "remote-copy-ready" on the
    # serial port): the clipboard must announce them, and Ctrl+V in an Explorer window must paste the file intact.
    Add-Type -AssemblyName Microsoft.VisualBasic
    $dest = 'C:\oxydesk\paste-remote'
    Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory $dest | Out-Null
    $seq = [Win32Clip]::GetClipboardSequenceNumber()
    Emit 'STEP remote-copy-ready'
    if ($p.remote_keys -eq '1') {
      # The host opens the file manager in the remote session meanwhile; Ctrl+C then goes through the keyboard of the control.
      $null = Pump { $false } 25000
      $form.Activate(); $null = $host_.Focus()
      [System.Windows.Forms.SendKeys]::SendWait('^c')
      Emit 'STEP ctrl-c sent'
    }
    $announced = Pump { [Win32Clip]::GetClipboardSequenceNumber() -ne $seq } 120000
    Check 'remote_copy_announced' $announced "clipboard sequence changed=$announced"
    if ($announced) {
      Start-Process explorer.exe -ArgumentList $dest
      $null = Pump { $false } 5000
      $activated = $true
      try { [Microsoft.VisualBasic.Interaction]::AppActivate('paste-remote') } catch { $activated = $false }
      $null = Pump { $false } 1000
      [System.Windows.Forms.SendKeys]::SendWait('^v')
      $target = Join-Path $dest $p.remote_name
      $null = Pump { (Test-Path -LiteralPath $target) -and (Get-Item -LiteralPath $target).Length -eq [int64]$p.remote_size } 30000
      $size = if (Test-Path -LiteralPath $target) { (Get-Item -LiteralPath $target).Length } else { -1 }
      Check 'remote_files_paste' ($size -eq [int64]$p.remote_size) "explorer activated=$activated, $($p.remote_name): $size bytes, expected $($p.remote_size)"
    }
    $ocx.Disconnect()
    $null = Pump { $state.disconnected } 10000
    Finish
  }

  # Frames: the synthetic desktop draws a heartbeat block at (32,0) that toggles white/dark.
  $seen = @{}
  $null = Pump { $c = PixelAt 48 16; $seen[[string]($c.R -gt 150)] = $true; $seen.Count -ge 2 } 10000
  $c = PixelAt 16 16
  $shot = 'C:\oxydesk\screen.png'
  $bmp = New-Object System.Drawing.Bitmap $w, $h
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($form.Location, [System.Drawing.Point]::Empty, $form.ClientSize); $g.Dispose()
  $bmp.Save($shot); $bmp.Dispose()
  if ($p.klid -eq '040C' -and $p.layout_check -ne '0') {
    # The synthetic desktop shows a French layout received from the client by an orange marker (255,128,0) at (0,0).
    function Orange($c) { [math]::Abs($c.R - 255) -lt 70 -and [math]::Abs($c.G - 128) -lt 70 -and $c.B -lt 70 }  # H.264 shifts colours
    $null = Pump { Orange (PixelAt 8 8) } 10000
    $m = PixelAt 8 8
    Check 'keyboard_layout' (Orange $m) "klid $($p.klid): marker pixel $($m.R),$($m.G),$($m.B), expected 255,128,0"
  }
  if ($p.reconnect -ne '1') { Check 'heartbeat' ($seen.Count -ge 2) "states seen: $($seen.Keys -join ',')" }

  if ($p.reconnect -eq '1') {
    # Network loss (the host resets the flow when it sees CUT): mstsc must reconnect by itself, without asking for the
    # password again, and the same desktop must come back (the heartbeat moves again, Connected is 1 at the end).
    Emit 'STEP cut-now'
    $null = Pump { $false } 6000
    $null = Pump { $state.connected } 60000
    # The login mode shows a real desktop (no heartbeat): the host checks in the server journal that the cookie was used.
    Check 'auto_reconnect' $state.connected "connected=$($state.connected) reason=$($state.reason)"
    $ocx.Disconnect()
    $null = Pump { $state.disconnected } 10000
    Finish
  }

  if ($p.drives -eq '1') {
    # The host (xtask) reads guest.bin through the mount, writes from-host.bin, hostdir\note.txt, deletes guest.txt, then
    # writes host-done.txt. The redirection is served by the control of this thread: keep pumping while waiting.
    $got = Pump { Test-Path "$drivesDir\host-done.txt" } 90000
    $detail = "host finished=$got"
    $okd = $got
    if ($got) {
      $v = [DrivePattern]::Verify("$drivesDir\from-host.bin", 2MB + 17)
      if ($v) { $okd = $false; $detail += ", from-host.bin: $v" } else { $detail += ', from-host.bin ok' }
      $note = if (Test-Path "$drivesDir\hostdir\note.txt") { [IO.File]::ReadAllText("$drivesDir\hostdir\note.txt", [Text.Encoding]::UTF8) } else { $null }
      if ($note -ne ('salut ' + [char]0xE9 + ' ' + [char]0xE0 + ' ' + [char]0xFC)) { $okd = $false; $detail += ", note.txt: '$note'" } else { $detail += ', note.txt ok' }
      if (Test-Path "$drivesDir\guest.txt") { $okd = $false; $detail += ', guest.txt not deleted' } else { $detail += ', guest.txt deleted' }
      if (-not (Test-Path "$drivesDir\renamed.bin") -or (Test-Path "$drivesDir\to-rename.bin")) { $okd = $false; $detail += ', rename not seen' } else { $detail += ', rename ok' }
    }
    Check 'drives_roundtrip' ([bool]$okd) $detail
  }

  if ($p.printers -in @('1','2')) {
    # The host (xtask) sends one job to the printer's socket: "Bonjour imprimante\n" then 100000 bytes 'A'..'Z' repeated.
    $head = [Text.Encoding]::ASCII.GetBytes("Bonjour imprimante`n")
    $total = $head.Length + 100000
    $ms = New-Object IO.MemoryStream
    $null = Pump { $accepting.IsCompleted } 90000
    if ($accepting.IsCompleted) {
      $stream = $accepting.Result.GetStream()
      $buf = New-Object byte[] 65536
      $idle = [Diagnostics.Stopwatch]::StartNew()
      while ($idle.ElapsedMilliseconds -lt 20000 -and ($ms.Length -lt $total -or $p.printers -eq '2')) {
        $t = $stream.ReadAsync($buf, 0, $buf.Length)
        if (-not (Pump { $t.IsCompleted } 20000)) { break }
        if ($t.Result -le 0) { break }
        $ms.Write($buf, 0, $t.Result); $idle.Restart()
      }
    }
    $b = $ms.ToArray()
    if ($p.printers -eq '2') {
      # What the PostScript driver of this machine made of the XPS document (the host converted a PostScript job).
      $all = [Text.Encoding]::ASCII.GetString($b) -replace '[^\x20-\x7e\r\n]', '.'
      $text = $all.Substring(0, [Math]::Min($all.Length, 400))
      $pages = ([regex]::Matches($all, '(?m)^%%Page: ')).Count
      Check 'printers_xps' ($b.Length -gt 1000 -and $all.Contains('%!PS') -and $pages -ge 2) ("received $($b.Length) bytes, $pages pages, start: $text")
    } else {
    $okp = ($b.Length -eq $total)
    $detail = "printed $($b.Length) bytes, expected $total"
    if ($okp) {
      for ($i = 0; $i -lt $head.Length; $i++) { if ($b[$i] -ne $head[$i]) { $okp = $false; $detail += ", head differs at $i"; break } }
      for ($i = 0; $okp -and $i -lt 100000; $i++) { if ($b[$head.Length + $i] -ne (65 + ($i % 26))) { $okp = $false; $detail += ", differs at $i"; break } }
    }
    Check 'printers_roundtrip' ([bool]$okp) $detail
    }
  }

  # Clipboard round trip. The test server echoes the text it receives: after our own SetText the
  # clipboard sequence number must change again (the echo announced by the server), and reading the
  # text must give the original back (data fetched from the server). CRLF normalized (Windows).
  function ClipboardRoundTrip([string]$name, [string]$text) {
    [System.Windows.Forms.Clipboard]::SetText($text)
    $seq = [Win32Clip]::GetClipboardSequenceNumber()
    $bumped = Pump { [Win32Clip]::GetClipboardSequenceNumber() -ne $seq } 10000
    # Read from a child process: the delayed rendering is served by the RDP control of this thread,
    # which must keep pumping messages (a GetText here would block it).
    $script:got = $null
    if ($bumped) {
      $out = 'C:\oxydesk\clip.txt'; Remove-Item $out -ErrorAction SilentlyContinue
      $cmd = '[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes([Windows.Forms.Clipboard]::GetText())) | Out-File ' + $out + ' -Encoding ascii'
      $pr = Start-Process powershell.exe -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile', '-STA', '-Command', "Add-Type -AssemblyName System.Windows.Forms; $cmd"
      $null = Pump { $pr.HasExited } 20000
      if ($pr.HasExited -and (Test-Path $out)) {
        $b64 = (Get-Content $out -Raw).Trim()
        $script:got = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($b64))
      } elseif (-not $pr.HasExited) { $pr.Kill() }
    }
    $okText = $script:got -and ($script:got.Replace("`r`n", "`n") -eq $text)
    Check $name ([bool]($bumped -and $okText)) "echo announced=$bumped, text intact=$([bool]$okText), sent $($text.Length) chars"
  }
  $euro = [char]0x20AC
  $short = 'Na' + [char]0xEF + 've caf' + [char]0xE9 + ' ' + [char]0x2014 + " 10 $euro " + [char]::ConvertFromUtf32(0x1F600) + ' ' + [char]0x65E5 + [char]0x672C
  ClipboardRoundTrip 'clipboard_roundtrip' $short
  $unit = "Handwritten: ${euro}uro, na" + [char]0xEF + 've, ' + [char]::ConvertFromUtf32(0x1F600) + ' ' + [char]0x2014 + " line`n"
  $large = New-Object System.Text.StringBuilder
  while ($large.Length -lt 64KB) { [void]$large.Append($unit) }
  ClipboardRoundTrip 'clipboard_roundtrip_large' $large.ToString()

  # Child STA PowerShell (encoded command, no quoting issues) while this thread keeps pumping: the delayed
  # rendering of the clipboard (text, image, files) is served by the RDP control of this thread.
  function RunChild([string]$code, [int]$ms) {
    $enc = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes("Add-Type -AssemblyName System.Windows.Forms, System.Drawing; `$ErrorActionPreference = 'Stop'; $code"))
    $pr = Start-Process powershell.exe -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile', '-STA', '-EncodedCommand', $enc
    $null = Pump { $pr.HasExited } $ms
    if (-not $pr.HasExited) { $pr.Kill(); return $false }
    return $true
  }
  # Image round trip: a 64x48 bitmap with a known pattern; the echo comes back as a DIB, read pixel by pixel.
  function ImageRoundTrip([string]$name, [int]$iw, [int]$ih) {
    $bmp = New-Object System.Drawing.Bitmap $iw, $ih
    for ($y = 0; $y -lt $ih; $y++) { for ($x = 0; $x -lt $iw; $x++) { $bmp.SetPixel($x, $y, [System.Drawing.Color]::FromArgb(($x * 4) % 256, ($y * 5) % 256, ($x * $y) % 256)) } }
    [System.Windows.Forms.Clipboard]::SetImage($bmp)
    $seq = [Win32Clip]::GetClipboardSequenceNumber()
    $bumped = Pump { [Win32Clip]::GetClipboardSequenceNumber() -ne $seq } 10000
    $detail = "echo announced=$bumped"
    $ok = $false
    if ($bumped) {
      $out = 'C:\oxydesk\clipimg.txt'; Remove-Item $out -ErrorAction SilentlyContinue
      $code = "`$i = [Windows.Forms.Clipboard]::GetImage(); if (`$i) { `$o = New-Object System.Collections.ArrayList; [void]`$o.Add(`"`$(`$i.Width)x`$(`$i.Height)`"); for (`$y = 0; `$y -lt `$i.Height; `$y++) { for (`$x = 0; `$x -lt `$i.Width; `$x++) { `$c = `$i.GetPixel(`$x, `$y); [void]`$o.Add(`"`$(`$c.R),`$(`$c.G),`$(`$c.B)`") } }; `$o -join ';' | Out-File '$out' -Encoding ascii }"
      $done = RunChild $code 60000
      if ($done -and (Test-Path $out)) {
        $parts = (Get-Content $out -Raw).Trim().Split(';')
        $bad = 0
        if ($parts[0] -ne "${iw}x${ih}") { $bad = -1; $detail += ", size $($parts[0])" }
        else {
          for ($y = 0; $y -lt $ih; $y++) { for ($x = 0; $x -lt $iw; $x++) {
            if ($parts[1 + $y * $iw + $x] -ne "$(($x * 4) % 256),$(($y * 5) % 256),$(($x * $y) % 256)") { $bad++ }
          } }
          $detail += ", $bad wrong pixels of $($iw * $ih)"
        }
        $ok = ($bad -eq 0)
      } else { $detail += ", image not read (child done=$done)" }
    }
    $bmp.Dispose()
    Check $name ([bool]($bumped -and $ok)) $detail
  }
  ImageRoundTrip 'clipboard_image_roundtrip' 37 23
  ImageRoundTrip 'clipboard_image_roundtrip_64x48' 64 48

  # File round trip: a folder (small files + a nested one) then one large file are put on the clipboard; the
  # server echoes the list back (FileGroupDescriptorW), and an Explorer paste (the user path: it fetches
  # the contents by ranges, FileContents) copies it into another folder; contents compared by SHA-256.
  Add-Type -TypeDefinition @'
public static class FileDiff {
  // First difference, number of differing 64 KiB blocks and the offsets of the first few of them.
  public static string Describe(string a, string b) {
    using (var fa = System.IO.File.OpenRead(a)) using (var fb = System.IO.File.OpenRead(b)) {
      var ba = new byte[65536]; var bb = new byte[65536]; long off = 0; int bad = 0; long first = -1; var list = new System.Text.StringBuilder();
      while (true) {
        int ra = fa.Read(ba, 0, ba.Length); int rb = fb.Read(bb, 0, bb.Length);
        if (ra <= 0 && rb <= 0) break;
        bool same = ra == rb; for (int i = 0; same && i < ra; i++) if (ba[i] != bb[i]) { same = false; if (first < 0) first = off + i; }
        if (!same) { bad++; if (bad <= 6) list.Append(off.ToString("X") + " "); }
        off += 65536;
      }
      return "first diff at " + first + ", " + bad + " bad 64K blocks, first at 0x " + list;
    }
  }
}
'@
  # Explorer creates the destination at its final size at once: the copy is finished when the file can be opened exclusively.
  function Unlocked([string]$f) { try { $h = [IO.File]::Open($f, 'Open', 'Read', 'None'); $h.Close(); return $true } catch { return $false } }
  function FilesRoundTrip([string]$name, [string[]]$paths, [string]$check, [int]$ms) {
    $dest = 'C:\oxydesk\paste'
    Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory $dest | Out-Null
    $sc = New-Object System.Collections.Specialized.StringCollection
    foreach ($f in $paths) { [void]$sc.Add($f) }
    [System.Windows.Forms.Clipboard]::SetFileDropList($sc)
    $seq = [Win32Clip]::GetClipboardSequenceNumber()
    $bumped = Pump { [Win32Clip]::GetClipboardSequenceNumber() -ne $seq } $ms
    $detail = "echo announced=$bumped"
    $ok = $false
    if ($bumped) {
      # The user path: an Explorer window on the destination folder, Ctrl+V. This thread keeps pumping messages
      # (the RDP control serves the delayed rendering and the FileContents requests of Explorer from it).
      Add-Type -AssemblyName Microsoft.VisualBasic
      Start-Process explorer.exe -ArgumentList $dest
      $null = Pump { $false } 5000
      $activated = $true
      try { [Microsoft.VisualBasic.Interaction]::AppActivate('paste') } catch { $activated = $false }
      $null = Pump { $false } 1000
      [System.Windows.Forms.SendKeys]::SendWait('^v')
      $detail += ", explorer window activated=$activated"
      $expected = $check.Split('|')
      $null = Pump { $all = $true; foreach ($e in $expected) { $src, $dst = $e.Split('>'); if (-not (Test-Path (Join-Path $dest $dst)) -or (Get-Item (Join-Path $dest $dst)).Length -ne (Get-Item $src).Length -or -not (Unlocked (Join-Path $dest $dst))) { $all = $false } }; $all } $ms
      $ok = $true
      foreach ($e in $expected) {
        $src, $dst = $e.Split('>'); $d = Join-Path $dest $dst
        if (-not (Test-Path $d)) { $ok = $false; $detail += ", missing $dst"; continue }
        $a = (Get-FileHash $src -Algorithm SHA256).Hash; $b = (Get-FileHash $d -Algorithm SHA256).Hash
        if ($a -ne $b) { $ok = $false; $detail += ", $dst differs ($((Get-Item $d).Length) bytes of $((Get-Item $src).Length)): $([FileDiff]::Describe($src, $d))" } else { $detail += ", $dst ok ($((Get-Item $d).Length) bytes)" }
      }
    }
    Check $name ([bool]($bumped -and $ok)) $detail
  }
  # Real-time scanning of a freshly written 128 MiB file can make Explorer's range reads fail (CB_RESPONSE_FAIL): not under test.
  try { Add-MpPreference -ExclusionPath 'C:\oxydesk' -ErrorAction Stop } catch { Emit ('DEFENDER EXCLUSION FAILED: ' + $_) }
  $fr = 'C:\oxydesk\files'
  Remove-Item $fr -Recurse -Force -ErrorAction SilentlyContinue
  New-Item -ItemType Directory "$fr\tree\sub" | Out-Null
  $rng = New-Object System.Random 42
  $buf = New-Object byte[] 70001; $rng.NextBytes($buf); [IO.File]::WriteAllBytes("$fr\tree\a.bin", $buf)
  [IO.File]::WriteAllText("$fr\tree\sub\caf" + [char]0xE9 + ".txt", "Na" + [char]0xEF + "ve " + [char]0x20AC + "`r`nline 2`r`n", [Text.Encoding]::UTF8)
  [IO.File]::WriteAllBytes("$fr\tree\empty.dat", (New-Object byte[] 0))
  FilesRoundTrip 'clipboard_files_roundtrip' @("$fr\tree") ("$fr\tree\a.bin>tree\a.bin|$fr\tree\sub\caf" + [char]0xE9 + ".txt>tree\sub\caf" + [char]0xE9 + ".txt|$fr\tree\empty.dat>tree\empty.dat") 90000
  $big = "$fr\big.bin"
  $fs = [IO.File]::Create($big); $chunk = New-Object byte[] (4MB)
  for ($i = 0; $i -lt [int]$p.big_mib / 4; $i++) { $rng.NextBytes($chunk); $fs.Write($chunk, 0, $chunk.Length) }
  $fs.Close()
  $null = Pump { Unlocked $big } 30000
  FilesRoundTrip 'clipboard_files_large' @($big) "$big>big.bin" 240000

  $snd = @(Get-CimInstance Win32_SoundDevice | ForEach-Object { "$($_.Name) [$($_.Status)]" })
  $svc = (Get-Service Audiosrv).Status
  Check 'guest_audio_device' ($snd.Count -gt 0 -and $svc -eq 'Running') "devices: $($snd -join '; '); Audiosrv $svc"
  # Audio: stay connected while the tone plays; QEMU records the guest output (wav backend) and the
  # host verifies the 440 Hz tone after the run (see win11.sh test).
  $null = Pump { $false } 6000
  Check 'still_connected_after_audio' $state.connected "Connected=$($ocx.Connected)"

  # Bottom-right corner block of the desktop (magenta): its position in the guest window gives the size
  # of the remote desktop. Scan a screenshot of the control for magenta pixels.
  function MagentaBox {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($form.Location, [System.Drawing.Point]::Empty, $form.ClientSize); $g.Dispose()
    $x0 = 99999; $y0 = 99999; $x1 = -1; $y1 = -1
    for ($y = 0; $y -lt $h; $y += 4) { for ($x = 0; $x -lt $w; $x += 4) {
      $c = $bmp.GetPixel($x, $y)
      if ($c.R -gt 200 -and $c.G -lt 60 -and $c.B -gt 200) {
        if ($x -lt $x0) { $x0 = $x }; if ($y -lt $y0) { $y0 = $y }; if ($x -gt $x1) { $x1 = $x }; if ($y -gt $y1) { $y1 = $y }
      }
    } }
    $bmp.Dispose()
    if ($x1 -lt 0) { return $null }
    return @($x0, $y0, $x1, $y1)
  }
  $boxBefore = MagentaBox
  # Dynamic resize (display control): the test desktop draws a magenta block in its bottom-right corner,
  # so the new size is visible in the guest pixels. The session must stay connected and keep delivering frames.
  $resize = 'accepted'
  try { $ocx.UpdateSessionDisplaySettings(1024, 576, 0, 0, 0, 100, 100) } catch { $resize = "refused ($($_.Exception.Message.Trim()))" }
  # The control centers a desktop smaller than its window: offsets of the 1024x576 desktop.
  $ox = [int](($w - 1024) / 2); $oy = [int](($h - 576) / 2)
  # WIN_EXPECT_RESIZE=0 (login mode): whether the desktop shrinks, and when, depends on the server; the heartbeat is then
  # looked for at the origin as well as where a centered 1024x576 desktop would put it.
  $ox0 = 0; $oy0 = 0
  if ($p.expect_resize -eq '0') { $ox0 = $ox; $oy0 = $oy; $ox = 0; $oy = 0 }
  $seen2 = @{}; $seen3 = @{}
  $null = Pump {
    $c = PixelAt (48 + $ox) (16 + $oy); $seen2[[string]($c.R -gt 150)] = $true
    if ($p.expect_resize -eq '0') { $c = PixelAt (48 + $ox0) (16 + $oy0); $seen3[[string]($c.R -gt 150)] = $true }
    $seen2.Count -ge 2 -or $seen3.Count -ge 2 -or -not $state.connected } 8000
  if ($seen3.Count -gt $seen2.Count) { $seen2 = $seen3 }
  $box = $null
  $null = Pump { $script:box = MagentaBox; ($box -and $box[2] -ge ($ox + 1024 - 8) -and $box[2] -lt ($ox + 1024) -and $box[3] -ge ($oy + 576 - 8) -and $box[3] -lt ($oy + 576)) -or -not $state.connected } 8000
  $cornerOk = [bool]($box -and $box[2] -ge ($ox + 1024 - 8) -and $box[2] -lt ($ox + 1024) -and $box[3] -ge ($oy + 576 - 8) -and $box[3] -lt ($oy + 576))
  Check 'resize_request_survives' ($state.connected -and $seen2.Count -ge 2) "request $resize, connected=$($state.connected), heartbeat states=$($seen2.Count)"
  if ($p.expect_resize -eq '0') { Emit 'STEP desktop size not checked (WIN_EXPECT_RESIZE=0)' } else { Check 'desktop_resized' $cornerOk "magenta block (x0,y0,x1,y1): before $($boxBefore -join ',') after $($box -join ',') expected right/bottom edge at $($ox + 1024),$($oy + 576) (1024x576 desktop)" }

  # Clean disconnect.
  $ocx.Disconnect()
  $ok = Pump { $state.disconnected } 10000
  Check 'disconnect' $ok "reason=$($state.reason)"
} catch {
  Check 'exception' $false ($_ | Out-String)
}
Finish
