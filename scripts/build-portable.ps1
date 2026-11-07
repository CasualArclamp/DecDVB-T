# made by CasualArclamp
# Builds single-file DecDVB-T executables for 64-bit Windows 10/11 that need nothing
# installed: the C runtime is linked statically, so no Visual C++ redistributable.
# By default every app is built; the results go to exe\ (git-ignored):
# decdvbt-gui.exe (receiver), decdvbt.exe (command line) and decdvbt-tx.exe
# (transmitter). All radio drivers (HackRF, Airspy R2 / HydraSDR) are pure Rust
# and on by default. After DecLAB's script.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-portable.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\build-portable.ps1 gui cli   # just some
#
# Builds in target\portable, so the usual target directory and its cache stay as they
# are.

# Note: with -File, PowerShell binds only the first bare word to a
# [string[]] parameter, so the names are taken from all remaining arguments.
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Apps)
if (-not $Apps -or $Apps.Count -eq 0) { $Apps = @("gui", "cli", "tx") }

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$triple = "x86_64-pc-windows-msvc"

# app -> (cargo package, exe name)
$all = [ordered]@{
    gui = @("decdvbt-gui", "decdvbt-gui.exe")
    cli = @("decdvbt-cli", "decdvbt.exe")
    tx  = @("decdvbt-tx-app", "decdvbt-tx.exe")
}
foreach ($a in $Apps) {
    if (-not $all.Contains($a)) { throw "unknown app '$a' (gui, cli, tx)" }
}

# With an explicit --target the flags reach only the executables, not build scripts
# and proc macros.
$env:RUSTFLAGS = "-C target-feature=+crt-static"
$env:CARGO_TARGET_DIR = Join-Path $root "target\portable"
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = "6" }

Push-Location $root
try {
    $pkgs = foreach ($a in $Apps) { "-p"; $all[$a][0] }
    cargo build --release --target $triple @pkgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    $out = Join-Path $root "exe"
    New-Item -ItemType Directory -Force $out | Out-Null
    foreach ($a in $Apps) {
        $exe = $all[$a][1]
        Copy-Item (Join-Path $env:CARGO_TARGET_DIR "$triple\release\$exe") $out -Force
        $mb = (Get-Item (Join-Path $out $exe)).Length / 1MB
        Write-Host ("{0,-16} {1,6:N1} MB" -f $exe, $mb)
    }
    Write-Host "written to $out"
} finally {
    Pop-Location
}
