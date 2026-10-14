while ($true) {
  $c = Get-NetTCPConnection -ErrorAction SilentlyContinue
  $states = ($c | Group-Object State | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join ' '
  $procs = ($c | Group-Object OwningProcess | Sort-Object Count -Descending | Select-Object -First 4 | ForEach-Object {
    $n = (Get-Process -Id $_.Name -ErrorAction SilentlyContinue).ProcessName; "$n($($_.Name))=$($_.Count)" }) -join ' '
  $tw = $c | Where-Object State -eq 'TimeWait'
  $twl = ($tw | Group-Object LocalPort | Sort-Object Count -Descending | Select-Object -First 3 | ForEach-Object { "L$($_.Name)=$($_.Count)" }) -join ' '
  $twr = ($tw | Group-Object RemotePort | Sort-Object Count -Descending | Select-Object -First 3 | ForEach-Object { "R$($_.Name)=$($_.Count)" }) -join ' '
  $udp = (Get-NetUDPEndpoint -ErrorAction SilentlyContinue | Measure-Object).Count
  "$(Get-Date -Format o) total=$($c.Count) udp=$udp $states | $procs | $twl $twr" | Out-File -Append -FilePath $env:RUNNER_TEMP\sockets.log
  Start-Sleep -Seconds 3
}
