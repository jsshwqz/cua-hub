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
function Merge-McpServer($Path, $Entry) {
    if (Test-Path $Path) {
        Copy-Item $Path "$Path.bak-$(Get-Date -Format yyyyMMdd-HHmmss)" -Force
        $raw = Get-Content $Path -Raw
        $cfg = if ([string]::IsNullOrWhiteSpace($raw)) { [pscustomobject]@{} } else { $raw | ConvertFrom-Json }
    } else {
        $cfg = [pscustomobject]@{}
    }
    if (-not $cfg.PSObject.Properties['mcpServers']) {
        $cfg | Add-Member -NotePropertyName mcpServers -NotePropertyValue ([pscustomobject]@{})
    }
    $cfg.mcpServers | Add-Member -NotePropertyName 'cua-hub' -NotePropertyValue $Entry -Force
    $json = $cfg | ConvertTo-Json -Depth 100
    [System.IO.File]::WriteAllText($Path, $json, (New-Object System.Text.UTF8Encoding($false)))
    Write-Host "mcp:    $Path"
}

# Qoder desktop reads settings.json; the legacy mcp.json is kept for the CLI.
$desktopCfg = Join-Path $env:USERPROFILE '.qoder\settings.json'
$desktopEntry = [pscustomobject]@{
    type = 'stdio'
    command = $exe
    args = @('serve')
    env = [pscustomobject]@{}
}
Merge-McpServer $desktopCfg $desktopEntry

$cliCfg = Join-Path $env:USERPROFILE '.qoder\mcp.json'
Merge-McpServer $cliCfg ([pscustomobject]@{ command = $exe; args = @('serve') })
Write-Host "Restart Qoder or open a new conversation to load the tools."
