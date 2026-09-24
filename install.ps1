# Installs usage-widget from this checkout, registers it to run at login, and
# starts it. Equivalent to:
#   cargo install --git https://github.com/pepsi-enjoyer/usage-widget
#   usage-widget --startup
#   usage-widget
# Run again after pulling changes to rebuild and restart.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path

# Reuse the checkout's build cache; by default cargo install rebuilds every
# dependency in a fresh temp directory, which takes 10+ minutes.
$env:CARGO_TARGET_DIR = Join-Path $root "target"

# A running exe cannot be replaced, so stop the widget before installing.
Get-Process usage-widget -ErrorAction SilentlyContinue | Stop-Process -Force

cargo install --path $root
if ($LASTEXITCODE -ne 0) { throw "cargo install failed" }

$exe = Join-Path $HOME ".cargo\bin\usage-widget.exe"
if (-not (Test-Path $exe)) { throw "expected $exe after cargo install" }

# Older versions of this script used a Startup-folder shortcut; drop it so the
# widget is not launched twice.
$oldLnk = Join-Path ([Environment]::GetFolderPath("Startup")) "usage-widget.lnk"
if (Test-Path $oldLnk) { Remove-Item $oldLnk }

$runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
Set-ItemProperty -Path $runKey -Name "usage-widget" -Value "`"$exe`"" -Type String

Start-Process -FilePath $exe

Write-Host "Installed $exe"
Write-Host "Registered to run at login (HKCU Run key 'usage-widget')."
