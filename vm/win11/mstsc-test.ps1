# Logon task of the test VM: if a data CD carries target.rdp (+ cred.txt = "user<TAB>password"),
# stores the credentials for the target host and opens the stock mstsc.exe on it.
# The script is fixed; the CD only provides data.
$log = 'C:\rdsktp\mstsc-test.log'
foreach ($d in Get-PSDrive -PSProvider FileSystem) {
  $rdp = Join-Path $d.Root 'target.rdp'
  $cred = Join-Path $d.Root 'cred.txt'
  if ((Test-Path $rdp) -and (Test-Path $cred)) {
    $user, $pass = (Get-Content $cred -Raw).Trim() -split "`t", 2
    $addr = ((Get-Content $rdp | Where-Object { $_ -like 'full address:s:*' }) -split ':s:', 2)[1]
    $host_ = $addr.Split(':')[0]
    cmdkey /generic:"TERMSRV/$host_" /user:$user /pass:$pass | Out-Null
    Copy-Item $rdp C:\rdsktp\target.rdp -Force
    "$(Get-Date -Format o) mstsc -> $addr" | Out-File $log -Append
    Start-Process mstsc.exe -ArgumentList 'C:\rdsktp\target.rdp'
    break
  }
}
