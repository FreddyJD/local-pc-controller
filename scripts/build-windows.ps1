$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$cmake = 'C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin'
$nasm = Join-Path $env:LOCALAPPDATA 'LocalnetBuildTools\nasm-3.01'
if (Test-Path (Join-Path $cmake 'cmake.exe')) { $env:PATH = "$cmake;$env:PATH" }
if (Test-Path (Join-Path $nasm 'nasm.exe')) { $env:PATH = "$nasm;$env:PATH" }
if (!(Get-Command cmake -ErrorAction SilentlyContinue) -or !(Get-Command nasm -ErrorAction SilentlyContinue)) {
    throw 'Install CMake and NASM and add them to PATH. Native SIMD JPEG encoding requires both at build time.'
}
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
