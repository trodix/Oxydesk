# Deterministic RDP test run in the Windows test VM, no human or LLM in the loop.
# Hosts Microsoft's RDP ActiveX control (mstscax.dll, the engine of mstsc.exe) in a WinForms window,
# connects to the target given by params.txt on the data CD and reports one JSON line per check on
# COM1 (QEMU: -serial file). Fixed script: the CD only provides data.
#   params.txt: key=value lines (server, port, user, password, width, height)
# The script first prints READY and waits for a params.txt (any drive); the host snapshots the VM at
# that point (savevm) and, for each test, restores it, swaps the data CD and the run starts at once.
$ErrorActionPreference = 'Stop'
trap { try { Emit ('ERROR ' + ($_ | Out-String).Replace("`r", '').Replace("`n", ' | ')) } catch { }; try { Start-Sleep 2; Stop-Computer -Force } catch { }; exit 1 }
$log = 'C:\rdsktp\activex-test.log'
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
Emit "BEGIN $(Get-Date -Format o) server=$($p.server):$($p.port)"
$w = [int]$p.width; $h = [int]$p.height

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
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
}
[System.Runtime.InteropServices.ComImport,
 System.Runtime.InteropServices.Guid("c1e6743a-41c1-4a74-832a-0dd06c1c7a0e"),
 System.Runtime.InteropServices.InterfaceType(System.Runtime.InteropServices.ComInterfaceType.InterfaceIsIUnknown)]
interface IMsTscNonScriptable {
  [System.Runtime.InteropServices.PreserveSig]
  int put_ClearTextPassword([System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.BStr)] string p);
}
public class RdpHost : System.Windows.Forms.AxHost {
  public RdpHost(string clsid) : base(clsid) {}
  public object Ocx { get { return GetOcx(); } }
  public int SetPassword(string p) { return ((IMsTscNonScriptable)GetOcx()).put_ClearTextPassword(p); }
}
"@

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

Emit 'STEP connecting'
try {
  $ocx.Connect(); $script:connectCalled = $true
  $ok = Pump { $state.connected -or $state.disconnected } 30000
  Check 'connect' ($ok -and $state.connected) ("connected=$($state.connected) disconnectReason=$($state.reason)")
  if (-not $state.connected) { Finish }

  # Frames: the synthetic desktop draws a heartbeat block at (32,0) that toggles white/dark.
  $seen = @{}
  $null = Pump { $c = PixelAt 48 16; $seen[[string]($c.R -gt 150)] = $true; $seen.Count -ge 2 } 10000
  $c = PixelAt 16 16
  $shot = 'C:\rdsktp\screen.png'
  $bmp = New-Object System.Drawing.Bitmap $w, $h
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($form.Location, [System.Drawing.Point]::Empty, $form.ClientSize); $g.Dispose()
  $bmp.Save($shot); $bmp.Dispose()
  Check 'heartbeat' ($seen.Count -ge 2) "states seen: $($seen.Keys -join ',')"

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
      $out = 'C:\rdsktp\clip.txt'; Remove-Item $out -ErrorAction SilentlyContinue
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
  if ($p.expect_resize -eq '0') { $ox = 0; $oy = 0 }   # desktop unchanged: still at the window origin
  $seen2 = @{}
  $null = Pump { $c = PixelAt (48 + $ox) (16 + $oy); $seen2[[string]($c.R -gt 150)] = $true; $seen2.Count -ge 2 -or -not $state.connected } 8000
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
