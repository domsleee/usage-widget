# Builds the release binary and creates a Startup-folder shortcut so the widget
# launches at login. Run again after pulling changes to rebuild.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path

Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}

$exe = Join-Path $root "target\release\usage-widget.exe"
$startup = [Environment]::GetFolderPath("Startup")
$lnk = Join-Path $startup "usage-widget.lnk"

$shell = New-Object -ComObject WScript.Shell
$sc = $shell.CreateShortcut($lnk)
$sc.TargetPath = $exe
$sc.WorkingDirectory = $root
$sc.Description = "Copilot / Claude / Codex usage widget"
$sc.Save()

Write-Host "Built $exe"
Write-Host "Startup shortcut: $lnk"

# Restart any running instance so the new build is what is on screen.
Get-Process usage-widget -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Process -FilePath $exe -WorkingDirectory $root
