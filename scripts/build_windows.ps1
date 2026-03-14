#!/usr/bin/env pwsh
# build_windows.ps1 - Build Hobbes for Windows
# Run this script from a PowerShell terminal on Windows
# Usage: .\scripts\build_windows.ps1 [-Sign] [-Release] [-Installer]

param(
    [switch]$Sign,       # Enable code signing (requires cert setup)
    [switch]$Release,    # Build release mode (default: debug for testing)
    [switch]$Installer,  # Build Inno Setup installer after compilation
    [switch]$Help
)

$ErrorActionPreference = "Stop"

if ($Help) {
    Write-Host @"
Hobbes Windows Build Script
===========================
Usage: .\scripts\build_windows.ps1 [options]

Options:
  -Release    Build in release mode (optimized, slower compile)
  -Sign       Sign the executable (requires HOBBES_WIN_CERT_THUMBPRINT env var)
  -Installer  Build Inno Setup installer (requires ISCC.exe on PATH or in default location)
  -Help       Show this help message

Environment Variables:
  HOBBES_WIN_CERT_THUMBPRINT   SHA1 thumbprint of code signing certificate
  HOBBES_WIN_TIMESTAMP_URL     Timestamp server (default: http://timestamp.digicert.com)

Prerequisites:
  1. Rust toolchain: rustup target add x86_64-pc-windows-msvc
  2. Visual Studio Build Tools (C++ workload)
  3. Windows SDK (for signtool, if signing)
  4. Inno Setup 6 (for -Installer flag): https://jrsoftware.org/isdl.php
  5. Node.js + npm (for tailwindcss build)

Examples:
  .\scripts\build_windows.ps1              # Debug build, no signing
  .\scripts\build_windows.ps1 -Release     # Release build, no signing
  .\scripts\build_windows.ps1 -Release -Sign -Installer  # Full release pipeline
"@
    exit 0
}

# Paths
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectDir = Split-Path -Parent $ScriptDir
$WindowsAppDir = Join-Path $ProjectDir "apps\windows_app"

Set-Location $ProjectDir

Write-Host ""
Write-Host "=== Hobbes Windows Build ===" -ForegroundColor Cyan
Write-Host ""

# Check prerequisites
Write-Host "=== Checking Prerequisites ===" -ForegroundColor Yellow

# Check Rust
if (-not (Get-Command "cargo" -ErrorAction SilentlyContinue)) {
    Write-Host "  [X] Rust not found. Install from https://rustup.rs" -ForegroundColor Red
    exit 1
}
Write-Host "  [OK] Rust: $(rustc --version)" -ForegroundColor Green

# Check target
$targets = rustup target list --installed
if ($targets -notcontains "x86_64-pc-windows-msvc") {
    Write-Host "  [!] Adding x86_64-pc-windows-msvc target..." -ForegroundColor Yellow
    rustup target add x86_64-pc-windows-msvc
}
Write-Host "  [OK] Target: x86_64-pc-windows-msvc" -ForegroundColor Green

# Check Node.js (needed for tailwindcss)
if (-not (Get-Command "node" -ErrorAction SilentlyContinue)) {
    Write-Host "  [!] Node.js not found. Install from https://nodejs.org" -ForegroundColor Yellow
    Write-Host "      Tailwind CSS build may fail without it." -ForegroundColor DarkGray
} else {
    Write-Host "  [OK] Node.js: $(node --version)" -ForegroundColor Green
}

# Version from Cargo.toml
$CargoToml = Get-Content (Join-Path $ProjectDir "Cargo.toml") -Raw
if ($CargoToml -match 'version\s*=\s*"([^"]+)"') {
    $Version = $matches[1]
} else {
    $Version = "0.0.0"
}
Write-Host "  [OK] Version: $Version" -ForegroundColor Green

# Check npm dependencies
if (-not (Test-Path (Join-Path $ProjectDir "node_modules"))) {
    Write-Host "  [!] Installing npm dependencies (tailwindcss)..." -ForegroundColor Yellow
    & npm ci
}

Write-Host ""
Write-Host "=== Building ===" -ForegroundColor Yellow

$BuildMode = if ($Release) { "--release" } else { "" }
$BuildModeLabel = if ($Release) { "release" } else { "debug" }

Write-Host "  Mode: $BuildModeLabel"
Write-Host "  Package: windows_app (binary: hobbes)"
Write-Host ""

# Build the Windows app crate (uses shared main.rs)
$buildArgs = @("build", "-p", "windows_app")
if ($Release) { $buildArgs += "--release" }

Write-Host "  > cargo $($buildArgs -join ' ')" -ForegroundColor DarkGray
& cargo @buildArgs

if ($LASTEXITCODE -ne 0) {
    Write-Host "  [X] Build failed!" -ForegroundColor Red
    exit 1
}

# Cargo produces hobbes.exe; rename to hobbes_VERSION.exe
$RawBinaryPath = Join-Path $ProjectDir "target\$BuildModeLabel\hobbes.exe"
if (-not (Test-Path $RawBinaryPath)) {
    Write-Host "  [X] Binary not found at: $RawBinaryPath" -ForegroundColor Red
    exit 1
}

$VersionedName = "hobbes_$Version.exe"
$BinaryPath = Join-Path $ProjectDir "target\$BuildModeLabel\$VersionedName"
Move-Item -Force $RawBinaryPath $BinaryPath

$BinarySize = "{0:N2} MB" -f ((Get-Item $BinaryPath).Length / 1MB)
Write-Host "  [OK] Built: $VersionedName ($BinarySize)" -ForegroundColor Green

# =====================================================================
# Signing (optional)
# =====================================================================
# Helper function to find signtool.exe
function Find-SignTool {
    $SignTool = Get-ChildItem -Path "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter "signtool.exe" -ErrorAction SilentlyContinue | 
                Sort-Object FullName -Descending | 
                Select-Object -First 1
    return $SignTool
}

function Sign-Binary {
    param([string]$FilePath, [string]$Label)
    
    $CertThumbprint = $env:HOBBES_WIN_CERT_THUMBPRINT
    $TimestampUrl = if ($env:HOBBES_WIN_TIMESTAMP_URL) { $env:HOBBES_WIN_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }
    
    if (-not $CertThumbprint) {
        Write-Host "  [!] HOBBES_WIN_CERT_THUMBPRINT not set. Skipping signing for: $Label" -ForegroundColor Yellow
        return $false
    }
    
    $SignTool = Find-SignTool
    if (-not $SignTool) {
        Write-Host "  [X] signtool.exe not found. Install Windows SDK." -ForegroundColor Red
        return $false
    }
    
    Write-Host "  Signing $Label..." -ForegroundColor DarkGray
    & $SignTool.FullName sign /fd SHA256 /tr $TimestampUrl /td SHA256 /sha1 $CertThumbprint $FilePath
    
    if ($LASTEXITCODE -ne 0) {
        Write-Host "  [X] Signing failed for: $Label" -ForegroundColor Red
        return $false
    }
    
    # Verify
    & $SignTool.FullName verify /pa $FilePath | Out-Null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "  [OK] Signed and verified: $Label" -ForegroundColor Green
    }
    return $true
}

if ($Sign) {
    Write-Host ""
    Write-Host "=== Code Signing (Binary) ===" -ForegroundColor Yellow
    Sign-Binary -FilePath $BinaryPath -Label $VersionedName
}

# =====================================================================
# Installer (optional, requires Inno Setup)
# =====================================================================
if ($Installer) {
    Write-Host ""
    Write-Host "=== Building Installer ===" -ForegroundColor Yellow
    
    # Find ISCC.exe (Inno Setup Compiler)
    $ISCC = $null
    
    # Check PATH first
    if (Get-Command "ISCC" -ErrorAction SilentlyContinue) {
        $ISCC = (Get-Command "ISCC").Source
    }
    
    # Check default install locations
    if (-not $ISCC) {
        $DefaultPaths = @(
            "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
            "C:\Program Files\Inno Setup 6\ISCC.exe"
        )
        foreach ($path in $DefaultPaths) {
            if (Test-Path $path) {
                $ISCC = $path
                break
            }
        }
    }
    
    if (-not $ISCC) {
        Write-Host "  [X] ISCC.exe not found!" -ForegroundColor Red
        Write-Host "      Install Inno Setup 6 from: https://jrsoftware.org/isdl.php" -ForegroundColor DarkGray
        exit 1
    }
    
    Write-Host "  Using: $ISCC" -ForegroundColor DarkGray
    
    $IssFile = Join-Path $ScriptDir "hobbes_installer.iss"
    if (-not (Test-Path $IssFile)) {
        Write-Host "  [X] Installer script not found: $IssFile" -ForegroundColor Red
        exit 1
    }
    
    # Inno Setup compiles from the .iss directory context
    Write-Host "  > ISCC /DVersion=`"$Version`" $IssFile" -ForegroundColor DarkGray
    & $ISCC /DVersion="$Version" $IssFile
    
    if ($LASTEXITCODE -ne 0) {
        Write-Host "  [X] Installer build failed!" -ForegroundColor Red
        exit 1
    }
    
    # Find the output installer (Inno Setup puts it in scripts/Output/)
    $InstallerPath = Join-Path $ScriptDir "Output\hobbes_${Version}_setup.exe"
    if (Test-Path $InstallerPath) {
        $InstallerSize = "{0:N2} MB" -f ((Get-Item $InstallerPath).Length / 1MB)
        Write-Host "  [OK] Installer: hobbes_${Version}_setup.exe ($InstallerSize)" -ForegroundColor Green
        
        # Sign the installer too
        if ($Sign) {
            Write-Host ""
            Write-Host "=== Code Signing (Installer) ===" -ForegroundColor Yellow
            Sign-Binary -FilePath $InstallerPath -Label "hobbes_${Version}_setup.exe"
        }
    } else {
        Write-Host "  [!] Installer not found at expected path: $InstallerPath" -ForegroundColor Yellow
        Write-Host "      Check Inno Setup OutputDir setting." -ForegroundColor DarkGray
    }
}

# =====================================================================
# Summary
# =====================================================================
Write-Host ""
Write-Host "=== Build Complete ===" -ForegroundColor Green
Write-Host ""
Write-Host "  Binary:    $BinaryPath" -ForegroundColor Cyan
if ($Installer) {
    $InstallerPath = Join-Path $ScriptDir "Output\hobbes_${Version}_setup.exe"
    if (Test-Path $InstallerPath) {
        Write-Host "  Installer: $InstallerPath" -ForegroundColor Cyan
    }
}
Write-Host ""

# Platform-specific notes
Write-Host "=== Windows Notes ===" -ForegroundColor Yellow
if (-not $Sign) {
    Write-Host "  - First run may trigger SmartScreen (unsigned binary)" -ForegroundColor DarkGray
    Write-Host "    Use -Sign flag with HOBBES_WIN_CERT_THUMBPRINT to avoid this" -ForegroundColor DarkGray
}
Write-Host "  - Credentials stored via Windows Credential Manager" -ForegroundColor DarkGray
Write-Host "  - MCP servers spawn via cmd.exe subprocess" -ForegroundColor DarkGray
Write-Host "  - Requires Edge WebView2 Runtime (installer handles this)" -ForegroundColor DarkGray
Write-Host ""
