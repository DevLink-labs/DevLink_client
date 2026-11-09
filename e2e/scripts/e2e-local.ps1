<#
.SYNOPSIS
  Runs the client·server·frontend E2E suite on local Windows (S15P31D108-179).

.DESCRIPTION
  1. Checks out DevLink_server and DevLink_frontend at the SHAs in e2e/stack.lock.json under the stack
     directory (shallow fetch by SHA), then builds central (bootJar) and the frontend (dist, empty
     VITE_API_BASE_URL) once per SHA.
  2. Builds this checkout's devlink CLI (release) and packages/mcp-server.
  3. Uses portable PostgreSQL 16 binaries (-PgBin, or <stack>/postgres16 prepared by -InstallPostgres,
     which downloads the pinned EDB zip and checks its SHA-256).
  4. Runs Playwright; global setup starts PostgreSQL/central/proxy on random loopback ports per run.

  No password or token is written by this script. Requires Node 24, JDK 21, Rust (rust-toolchain.toml), git.

.EXAMPLE
  pwsh e2e/scripts/e2e-local.ps1 -InstallPostgres
  pwsh e2e/scripts/e2e-local.ps1 -Runs 5
  pwsh e2e/scripts/e2e-local.ps1 -SkipBuild -Grep "S3"
#>
[CmdletBinding()]
param(
  [string]$StackDir,
  [string]$PgBin,
  [string]$Java,
  [switch]$InstallPostgres,
  [switch]$SkipBuild,
  [int]$Runs = 1,
  [string]$Grep
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$e2eRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $e2eRoot
if (-not $StackDir) { $StackDir = Join-Path $e2eRoot '.stack' }
$lock = Get-Content -LiteralPath (Join-Path $e2eRoot 'stack.lock.json') -Raw | ConvertFrom-Json
New-Item -ItemType Directory -Force -Path $StackDir | Out-Null

function Invoke-Checked([string]$Label, [scriptblock]$Command) {
  & $Command
  if ($LASTEXITCODE -ne 0) { throw "$Label failed with exit code $LASTEXITCODE" }
}

function Sync-Checkout([string]$Name, $Entry) {
  $dir = Join-Path $StackDir $Name
  if (-not (Test-Path -LiteralPath (Join-Path $dir '.git'))) {
    Invoke-Checked "git init $Name" { git init -q $dir }
  }
  # Check before changing revisions so a reused stack never discards local work.
  $status = git -C $dir status --porcelain
  if ($LASTEXITCODE -ne 0) { throw "[$Name] cannot inspect $dir" }
  if ($status) { throw "[$Name] $dir has local changes; the pinned checkout must be clean." }
  $head = (git -C $dir rev-parse -q --verify HEAD 2>$null)
  if ($head -ne $Entry.commit) {
    Write-Host "[$Name] fetching $($Entry.commit)"
    Invoke-Checked "git fetch $Name" { git -C $dir fetch -q --depth 1 $Entry.repository $Entry.commit }
    Invoke-Checked "git checkout $Name" { git -C $dir checkout -q --detach --no-overwrite-ignore FETCH_HEAD }
  }
  # Build outputs (build/, dist/, node_modules/) are ignored by each repo; any other change is not the pinned source.
  if (git -C $dir status --porcelain) { throw "[$Name] $dir has local changes; the pinned checkout must be clean." }
  return $dir
}

function Test-Built([string]$Marker, [string]$Commit) {
  (Test-Path -LiteralPath $Marker) -and ((Get-Content -LiteralPath $Marker -Raw).Trim() -eq $Commit)
}

# --- PostgreSQL 16 ---------------------------------------------------------------------------
if (-not $PgBin) { $PgBin = Join-Path $StackDir 'postgres16\pgsql\bin' }
if (-not (Test-Path -LiteralPath (Join-Path $PgBin 'initdb.exe'))) {
  if (-not $InstallPostgres) {
    throw "PostgreSQL 16 binaries not found at $PgBin. Pass -PgBin <dir with initdb.exe> or -InstallPostgres."
  }
  $pg = $lock.postgres.windowsBinaries
  $download = Join-Path $StackDir 'downloads'
  New-Item -ItemType Directory -Force -Path $download | Out-Null
  $zip = Join-Path $download "postgresql-$($pg.version)-windows-x64-binaries.zip"
  if (-not (Test-Path -LiteralPath $zip)) {
    Write-Host "[postgres] downloading $($pg.url)"
    Invoke-WebRequest -Uri $pg.url -OutFile $zip
  }
  $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($hash -ne $pg.sha256) { Remove-Item -LiteralPath $zip; throw "PostgreSQL zip SHA-256 mismatch ($hash)" }
  $target = Join-Path $StackDir 'postgres16'
  Expand-Archive -LiteralPath $zip -DestinationPath $target -Force
  $PgBin = Join-Path $target 'pgsql\bin'
}
$pgVersion = & (Join-Path $PgBin 'postgres.exe') --version
if ($pgVersion -notmatch '\(PostgreSQL\) 16\.') { throw "PostgreSQL 16 required, found: $pgVersion" }

# --- sources and builds ----------------------------------------------------------------------
$serverDir = Sync-Checkout 'server' $lock.server
$frontendDir = Sync-Checkout 'frontend' $lock.frontend
# Vite reads ignored .env*.local files; one with a remote API URL must never get baked into dist.
foreach ($local in @('.env.local', '.env.production.local')) {
  if (Test-Path -LiteralPath (Join-Path $frontendDir $local)) { throw "Remove $(Join-Path $frontendDir $local): the E2E build must use the same-origin /api proxy." }
}

if (-not $SkipBuild) {
  $serverMarker = Join-Path $serverDir 'build\e2e-built-commit'
  if (-not (Test-Built $serverMarker (git -C $serverDir rev-parse HEAD)) -or -not (Test-Path (Join-Path $serverDir 'build\libs\app.jar'))) {
    Write-Host '[server] gradlew bootJar -x test'
    Push-Location $serverDir
    try { Invoke-Checked 'server build' { .\gradlew.bat bootJar -x test --no-daemon -q } } finally { Pop-Location }
    Set-Content -LiteralPath $serverMarker -Value (git -C $serverDir rev-parse HEAD)
  }

  $frontendMarker = Join-Path $frontendDir 'dist\.e2e-built-commit'
  if (-not (Test-Built $frontendMarker (git -C $frontendDir rev-parse HEAD))) {
    Write-Host '[frontend] npm ci && npm run build'
    Push-Location $frontendDir
    try {
      Invoke-Checked 'frontend npm ci' { npm ci --no-audit --no-fund }
      $env:VITE_API_BASE_URL = ''
      Invoke-Checked 'frontend build' { npm run build }
    } finally { Remove-Item Env:VITE_API_BASE_URL -ErrorAction SilentlyContinue; Pop-Location }
    Set-Content -LiteralPath $frontendMarker -Value (git -C $frontendDir rev-parse HEAD)
  }

  Write-Host '[client] cargo build -p devlink-cli --release --locked'
  Push-Location (Join-Path $repoRoot 'rust')
  try { Invoke-Checked 'cli build' { cargo build -p devlink-cli --release --locked } } finally { Pop-Location }

  Write-Host '[client] packages/mcp-server npm ci && npm run build'
  Push-Location (Join-Path $repoRoot 'packages\mcp-server')
  try {
    if (-not (Test-Path 'node_modules')) { Invoke-Checked 'mcp npm ci' { npm ci --no-audit --no-fund } }
    Invoke-Checked 'mcp build' { npm run build }
  } finally { Pop-Location }
}

# --- Playwright ------------------------------------------------------------------------------
Push-Location $e2eRoot
try {
  if (-not (Test-Path 'node_modules')) { Invoke-Checked 'e2e npm ci' { npm ci --no-audit --no-fund } }
  Invoke-Checked 'playwright install' { npx playwright install chromium }

  $env:E2E_STACK_DIR = $StackDir
  $env:E2E_SERVER_DIR = $serverDir
  $env:E2E_FRONTEND_DIR = $frontendDir
  $env:E2E_PG_BIN = $PgBin
  $env:E2E_DEVLINK_CLI = Join-Path $repoRoot 'rust\target\release\devlink.exe'
  $env:E2E_MCP_PACKAGE = Join-Path $repoRoot 'packages\mcp-server'
  if ($Java) { $env:E2E_JAVA = $Java }

  $playwrightArgs = @('playwright', 'test')
  if ($Grep) { $playwrightArgs += @('--grep', $Grep) }
  for ($run = 1; $run -le $Runs; $run++) {
    Write-Host "[e2e] run $run/$Runs"
    & npx @playwrightArgs
    if ($LASTEXITCODE -ne 0) { throw "E2E run $run/$Runs failed (exit $LASTEXITCODE). See e2e/playwright-report and $StackDir\runs." }
  }
  Write-Host "[e2e] $Runs consecutive run(s) passed"
} finally {
  Pop-Location
}
