param(
    [string]$Repo = (Resolve-Path "$PSScriptRoot\..").Path,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$dist = Join-Path $Repo 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null

if (-not $SkipBuild) {
    Push-Location $Repo
    try { cargo build --release } finally { Pop-Location }
}

$exe = Join-Path $dist 'cua-hub.exe'
Copy-Item (Join-Path $Repo 'target\release\cua-hub.exe') $exe -Force
Write-Host "binary: $exe"

# Skill for Qoder
$skillDir = Join-Path $env:USERPROFILE '.qoder\skills\cua-hub'
New-Item -ItemType Directory -Force $skillDir | Out-Null
Copy-Item (Join-Path $Repo 'integrations\skill\SKILL.md') (Join-Path $skillDir 'SKILL.md') -Force
Write-Host "skill:  $skillDir"

# Skill for Claude Code, if present
$claudeSkills = Join-Path $env:USERPROFILE '.claude\skills'
if (Test-Path $claudeSkills) {
    $claudeDir = Join-Path $claudeSkills 'cua-hub'
    New-Item -ItemType Directory -Force $claudeDir | Out-Null
    Copy-Item (Join-Path $Repo 'integrations\skill\SKILL.md') (Join-Path $claudeDir 'SKILL.md') -Force
    Write-Host "skill:  $claudeDir"
}

# MCP registration (merge, keep existing servers)
$cfgPath = Join-Path $env:USERPROFILE '.qoder\mcp.json'
if (Test-Path $cfgPath) {
    Copy-Item $cfgPath "$cfgPath.bak-$(Get-Date -Format yyyyMMdd-HHmmss)" -Force
    $cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json
} else {
    $cfg = [pscustomobject]@{}
}
if (-not $cfg.PSObject.Properties['mcpServers']) {
    $cfg | Add-Member -NotePropertyName mcpServers -NotePropertyValue ([pscustomobject]@{})
}
$entry = [pscustomobject]@{ command = $exe; args = @('serve') }
$cfg.mcpServers | Add-Member -NotePropertyName 'cua-hub' -NotePropertyValue $entry -Force
$json = $cfg | ConvertTo-Json -Depth 10
[System.IO.File]::WriteAllText($cfgPath, $json, (New-Object System.Text.UTF8Encoding($false)))
Write-Host "mcp:    $cfgPath (restart Qoder to load)"
