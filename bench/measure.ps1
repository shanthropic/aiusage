# One measured run: wall time and sampled memory of the node processes it starts.
#
# Memory is sampled, not read from a peak counter: nodeSumPeakMB is the highest
# instantaneous working set of all node.exe processes started by this run
# (npx/npm plus the CLI), nodeMaxPeakMB is the highest single process.
param(
  [Parameter(Mandatory = $true)][string]$RunCommand,
  [Parameter(Mandatory = $true)][string]$OutFile,
  [Parameter(Mandatory = $true)][string]$ErrFile,
  [Parameter(Mandatory = $true)][string]$ResultFile,
  [Parameter(Mandatory = $true)][int]$HarnessPid,
  [Parameter(Mandatory = $true)][string]$WorkingDirectory,
  [int]$TimeoutSeconds = 600,
  [int]$SampleMs = 20
)

$ErrorActionPreference = 'Stop'
$shell = if ($env:ComSpec) { $env:ComSpec } else { 'cmd.exe' }

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $shell
$psi.Arguments = '/d /s /c "' + $RunCommand + '"'
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.WorkingDirectory = $WorkingDirectory

$proc = New-Object System.Diagnostics.Process
$proc.StartInfo = $psi

$startedAt = (Get-Date).AddMilliseconds(-100)
$clock = [System.Diagnostics.Stopwatch]::StartNew()
$null = $proc.Start()
$proc.StandardInput.Close()

$outStream = [System.IO.File]::Create($OutFile)
$errStream = [System.IO.File]::Create($ErrFile)
$outCopy = $proc.StandardOutput.BaseStream.CopyToAsync($outStream)
$errCopy = $proc.StandardError.BaseStream.CopyToAsync($errStream)

$sumPeak = 0.0
$singlePeak = 0.0
$samples = 0
$timedOut = $false

while (-not $proc.HasExited) {
  if ($clock.Elapsed.TotalSeconds -gt $TimeoutSeconds) {
    $timedOut = $true
    & taskkill.exe /PID $proc.Id /T /F 2>$null | Out-Null
    break
  }
  $sum = 0.0
  foreach ($node in @(Get-Process -Name node -ErrorAction SilentlyContinue)) {
    if ($node.Id -eq $HarnessPid) { continue }
    try { $procStart = $node.StartTime } catch { continue }
    if ($procStart -lt $startedAt) { continue }
    $ws = [double]$node.WorkingSet64
    $sum += $ws
    if ($ws -gt $singlePeak) { $singlePeak = $ws }
  }
  if ($sum -gt $sumPeak) { $sumPeak = $sum }
  $samples++
  Start-Sleep -Milliseconds $SampleMs
}

$proc.WaitForExit()
$clock.Stop()
$null = $outCopy.Wait(10000)
$null = $errCopy.Wait(10000)
$outStream.Close()
$errStream.Close()

$exitCode = $null
if (-not $timedOut) {
  try { $exitCode = $proc.ExitCode } catch { $exitCode = $null }
}

[ordered]@{
  runCommand    = $RunCommand
  exitCode      = $exitCode
  timedOut      = $timedOut
  wallMs        = [math]::Round($clock.Elapsed.TotalMilliseconds, 3)
  nodeSumPeakMB = [math]::Round($sumPeak / 1MB, 3)
  nodeMaxPeakMB = [math]::Round($singlePeak / 1MB, 3)
  samples       = $samples
} | ConvertTo-Json -Compress | Set-Content -Path $ResultFile -Encoding ascii
