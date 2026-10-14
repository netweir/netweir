while ($true) {
  $c = Get-NetTCPConnection -ErrorAction SilentlyContinue
  $states = ($c | Group-Object State | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join ' '
  $procs = ($c | Group-Object OwningProcess | Sort-Object Count -Descending | Select-Object -First 4 | ForEach-Object {
    $n = (Get-Process -Id $_.Name -ErrorAction SilentlyContinue).ProcessName; "$n($($_.Name))=$($_.Count)" }) -join ' '
  $tw = $c | Where-Object State -eq 'TimeWait'
  $twl = ($tw | Group-Object LocalPort | Sort-Object Count -Descending | Select-Object -First 3 | ForEach-Object { "L$($_.Name)=$($_.Count)" }) -join ' '
  $twr = ($tw | Group-Object RemotePort | Sort-Object Count -Descending | Select-Object -First 3 | ForEach-Object { "R$($_.Name)=$($_.Count)" }) -join ' '
  $udp = (Get-NetUDPEndpoint -ErrorAction SilentlyContinue | Measure-Object).Count
  $ps = Get-Process
  $chrome = ($ps | Where-Object ProcessName -eq 'chrome' | Measure-Object).Count
  $handles = ($ps | Measure-Object -Property HandleCount -Sum).Sum
  $top = ($ps | Sort-Object HandleCount -Descending | Select-Object -First 3 | ForEach-Object { "$($_.ProcessName)($($_.Id))h=$($_.HandleCount)" }) -join ' '
  $os = Get-CimInstance Win32_PerfRawData_PerfOS_Memory
  $np = [math]::Round($os.PoolNonpagedBytes / 1MB)
  $free = [math]::Round($os.AvailableMBytes)
  "$(Get-Date -Format o) total=$($c.Count) udp=$udp $states | chrome=$chrome npMB=$np availMB=$free handles=$handles $top | $procs | $twl $twr" | Out-File -Append -FilePath $env:RUNNER_TEMP\sockets.log
  Start-Sleep -Seconds 1
}
