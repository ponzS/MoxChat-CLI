$ErrorActionPreference = 'Stop'
if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
    throw 'Enable WSL2 first: run wsl --install -d Ubuntu in administrator PowerShell, restart when prompted, then run this installer again.'
}
& wsl.exe -- sh -lc 'command -v sh >/dev/null'
if ($LASTEXITCODE -ne 0) {
    throw 'Set up your Ubuntu user in WSL2 first. If Ubuntu is missing, run wsl --install -d Ubuntu, then run this installer again.'
}
& wsl.exe -- bash -o pipefail -lc 'curl --proto "=https" --tlsv1.2 -fsSL https://raw.githubusercontent.com/ponzS/MoxChat-CLI/main/scripts/mox-cli-install.sh | sh'
if ($LASTEXITCODE -ne 0) { throw 'Mox installation failed inside WSL2.' }
Write-Host 'Open your Ubuntu terminal to run mox login and mox start. Install and sign in to Codex inside the same WSL2 environment.'
