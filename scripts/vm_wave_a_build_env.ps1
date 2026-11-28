# This file is dot-sourced in the dedicated guest before running Cargo.
if ($env:COMPUTERNAME -ne 'ASR-EVAL') { throw 'The build checks require the ASR-Eval guest.' }

$waveTools = 'C:\ASR-Evaluation\tools'
$waveVs = Join-Path $waveTools 'VSBuildTools\Common7\Tools\VsDevCmd.bat'
$env:CARGO_HOME = Join-Path $waveTools 'Rust\cargo'
$env:RUSTUP_HOME = Join-Path $waveTools 'Rust\rustup'
$env:CARGO_TARGET_DIR = 'C:\ASR-Evaluation\build-cache\wave-a-rust-1.97.1'
$waveCargo = Join-Path $env:CARGO_HOME 'bin\cargo.exe'
if (!(Test-Path -LiteralPath $waveVs -PathType Leaf) -or !(Test-Path -LiteralPath $waveCargo -PathType Leaf)) {
    throw 'Guest Rust and Visual Studio Build Tools must be provisioned first.'
}

$waveEnvironment = & "$env:SystemRoot\System32\cmd.exe" /d /c "call `"$waveVs`" -no_logo -arch=x64 -host_arch=x64 >nul && set"
if ($LASTEXITCODE -ne 0) { throw 'The guest x64 MSVC environment could not be initialized.' }
foreach ($waveLine in $waveEnvironment) {
    if ($waveLine -match '^([A-Za-z_][A-Za-z0-9_()]*)=(.*)$') {
        [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], 'Process')
    }
}
$env:Path = "$(Split-Path -Parent $waveCargo);$env:Path"
