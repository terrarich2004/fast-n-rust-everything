$ErrorActionPreference = "Stop"

$Repo = if ($env:FNR_REPO) { $env:FNR_REPO } else { "terrarich2004/fast-n-rust-everything" }
$InstallDir = if ($env:FNR_INSTALL_DIR) { $env:FNR_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".local\bin" }
$Version = $env:FNR_VERSION

if (-not $Version) {
  try {
    $Resp = Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest" -MaximumRedirection 0 -ErrorAction SilentlyContinue
    if ($Resp.Headers.Location -match "/tag/([^/]+)$") {
      $Version = $Matches[1]
    }
  } catch {
    if ($_.Exception.Response -and $_.Exception.Response.Headers["Location"] -match "/tag/([^/]+)$") {
      $Version = $Matches[1]
    }
  }
}
if (-not $Version) { $Version = "v0.2.0" }

$Target = "x86_64-pc-windows-msvc"
$Asset = "fast-n-rust-$Version-$Target.zip"
$BaseUrl = "https://github.com/$Repo/releases/download/$Version"
$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("fnr-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
  $ArchivePath = Join-Path $TmpDir $Asset
  Write-Host "Downloading fast-n-rust $Version ($Target)..."
  try {
    Invoke-WebRequest -Uri "$BaseUrl/$Asset" -OutFile $ArchivePath
  } catch {
    Write-Warning "Prebuilt binary $Asset was not found at $BaseUrl."
    Write-Host "Install directly from source with Rust/Cargo:"
    Write-Host "  cargo install --git https://github.com/$Repo fast-n-rust"
    exit 1
  }

  try {
    $SumsPath = Join-Path $TmpDir "checksums.txt"
    Invoke-WebRequest -Uri "$BaseUrl/checksums.txt" -OutFile $SumsPath
    $Line = Get-Content $SumsPath | Where-Object { $_ -match "\b$([regex]::Escape($Asset))$" } | Select-Object -First 1
    if ($Line -match "^([a-fA-F0-9]{64})\s+") {
      $Expected = $Matches[1].ToLower()
      $Actual = (Get-FileHash -Path $ArchivePath -Algorithm SHA256).Hash.ToLower()
      if ($Actual -ne $Expected) {
        throw "SHA-256 mismatch for $Asset"
      }
    }
  } catch {
    if ($_.Exception.Message -like "*SHA-256 mismatch*") { throw }
  }

  $Stage = Join-Path $TmpDir "stage"
  Expand-Archive -Path $ArchivePath -DestinationPath $Stage -Force
  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  $MainExe = Join-Path $InstallDir "fast-n-rust.exe"
  $AliasExe = Join-Path $InstallDir "fnr.exe"
  Copy-Item (Join-Path $Stage "fast-n-rust.exe") $MainExe -Force
  $StageAlias = Join-Path $Stage "fnr.exe"
  if (Test-Path $StageAlias) {
    Copy-Item $StageAlias $AliasExe -Force
  } else {
    try {
      if (Test-Path $AliasExe) { Remove-Item $AliasExe -Force }
      New-Item -ItemType HardLink -Path $AliasExe -Target $MainExe | Out-Null
    } catch {
      Copy-Item $MainExe $AliasExe -Force
    }
  }

  Write-Host "Installed fast-n-rust.exe and fnr.exe to $InstallDir"
  if ($env:PATH -notlike "*$InstallDir*") {
    Write-Host "Add $InstallDir to your PATH to run 'fnr' from anywhere."
  }
} finally {
  Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
