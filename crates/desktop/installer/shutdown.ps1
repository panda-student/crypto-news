param([Parameter(Mandatory=$true)][string]$ExecutablePath)
$ErrorActionPreference = 'Stop'
$targetExecutable = [IO.Path]::GetFullPath($ExecutablePath)
if (-not (Test-Path -LiteralPath $targetExecutable -PathType Leaf)) { exit 0 }
function Get-InstalledProcesses {
    @(Get-CimInstance Win32_Process -Filter "Name='desktop.exe'" | Where-Object {
        $_.ExecutablePath -and [string]::Equals([IO.Path]::GetFullPath($_.ExecutablePath), $targetExecutable, [StringComparison]::OrdinalIgnoreCase)
    })
}
try {
    $running = Get-InstalledProcesses
    if ($running.Count -eq 0) { exit 0 }
    $versionText = [Diagnostics.FileVersionInfo]::GetVersionInfo($targetExecutable).ProductVersion
    $versionMatch = [regex]::Match($versionText, '^\d+\.\d+\.\d+')
    if ($versionMatch.Success -and [version]$versionMatch.Value -ge [version]'0.2.0') {
        Start-Process -FilePath $targetExecutable -ArgumentList '--shutdown' -WindowStyle Hidden | Out-Null
    } else {
        foreach ($entry in $running) {
            $process = Get-Process -Id $entry.ProcessId -ErrorAction SilentlyContinue
            if ($process) { $null = $process.CloseMainWindow() }
        }
    }
    $graceDeadline = [DateTime]::UtcNow.AddSeconds(4)
    do {
        Start-Sleep -Milliseconds 200
        $running = Get-InstalledProcesses
    } while ($running.Count -gt 0 -and [DateTime]::UtcNow -lt $graceDeadline)
    # Older builds minimize to tray on CloseRequested. Force only verified installation PIDs.
    foreach ($entry in $running) { Stop-Process -Id $entry.ProcessId -Force -ErrorAction SilentlyContinue }
    $exitDeadline = [DateTime]::UtcNow.AddSeconds(8)
    do {
        $running = Get-InstalledProcesses
        if ($running.Count -eq 0) { exit 0 }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $exitDeadline)
    exit 1
} catch { exit 1 }
