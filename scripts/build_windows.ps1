#!/usr/bin/env pwsh
# build_windows.ps1 - Build Hobbes for Windows
# Run this script from a PowerShell terminal on Windows
# Usage: .\scripts\build_windows.ps1 [-Sign] [-Release]

param(
    [switch]$Sign,      # Enable code signing (requires cert setup)
    [switch]$Release,   # Build release mode (default: debug for testing)
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
  -Help       Show this help message

Environment Variables:
  HOBBES_WIN_CERT_THUMBPRINT   SHA1 thumbprint of code signing certificate
  HOBBES_WIN_TIMESTAMP_URL     Timestamp server (default: http://timestamp.digicert.com)

Prerequisites:
  1. Rust toolchain: rustup target add x86_64-pc-windows-msvc
  2. Visual Studio Build Tools (C++ workload)
  3. Windows SDK (for signtool, if signing)

Examples:
  .\scripts\build_windows.ps1              # Debug build, no signing
  .\scripts\build_windows.ps1 -Release     # Release build, no signing
  .\scripts\build_windows.ps1 -Release -Sign  # Release + signed
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

# Version from Cargo.toml
$CargoToml = Get-Content (Join-Path $ProjectDir "Cargo.toml") -Raw
if ($CargoToml -match 'version\s*=\s*"([^"]+)"') {
    $Version = $matches[1]
} else {
    $Version = "0.0.0"
}
Write-Host "  [OK] Version: $Version" -ForegroundColor Green

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

# Signing (optional)
if ($Sign) {
    Write-Host ""
    Write-Host "=== Code Signing ===" -ForegroundColor Yellow
    
    $CertThumbprint = $env:HOBBES_WIN_CERT_THUMBPRINT
    $TimestampUrl = if ($env:HOBBES_WIN_TIMESTAMP_URL) { $env:HOBBES_WIN_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }
    
    if (-not $CertThumbprint) {
        Write-Host "  [!] HOBBES_WIN_CERT_THUMBPRINT not set. Skipping signing." -ForegroundColor Yellow
        Write-Host "      To sign, set: `$env:HOBBES_WIN_CERT_THUMBPRINT = 'YOUR_CERT_SHA1'" -ForegroundColor DarkGray
    } else {
        # Find signtool
        $SignTool = Get-ChildItem -Path "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter "signtool.exe" -ErrorAction SilentlyContinue | 
                    Sort-Object FullName -Descending | 
                    Select-Object -First 1
        
        if (-not $SignTool) {
            Write-Host "  [X] signtool.exe not found. Install Windows SDK." -ForegroundColor Red
            exit 1
        }
        
        Write-Host "  Using: $($SignTool.FullName)" -ForegroundColor DarkGray
        Write-Host "  Certificate: $CertThumbprint" -ForegroundColor DarkGray
        Write-Host "  Timestamp: $TimestampUrl" -ForegroundColor DarkGray
        
        & $SignTool.FullName sign /fd SHA256 /tr $TimestampUrl /td SHA256 /sha1 $CertThumbprint $BinaryPath
        
        if ($LASTEXITCODE -ne 0) {
            Write-Host "  [X] Signing failed!" -ForegroundColor Red
            exit 1
        }
        
        Write-Host "  [OK] Signed successfully" -ForegroundColor Green
        
        # Verify
        & $SignTool.FullName verify /pa $BinaryPath | Out-Null
        if ($LASTEXITCODE -eq 0) {
            Write-Host "  [OK] Signature verified" -ForegroundColor Green
        }
    }
}

Write-Host ""
Write-Host "=== Build Complete ===" -ForegroundColor Green
Write-Host ""
Write-Host "Output: $BinaryPath" -ForegroundColor Cyan
Write-Host ""

# Platform-specific notes
Write-Host "=== Windows Notes ===" -ForegroundColor Yellow
Write-Host "  - First run may trigger SmartScreen (unsigned binary)" -ForegroundColor DarkGray
Write-Host "  - Credentials stored via Windows Credential Manager" -ForegroundColor DarkGray
Write-Host "  - MCP servers spawn via cmd.exe subprocess" -ForegroundColor DarkGray
Write-Host ""

