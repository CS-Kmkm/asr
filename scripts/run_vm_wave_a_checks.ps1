param(
    [string]$AppBinary,
    [string]$ManagementCredentialPath,
    [ValidateSet('Terminals', 'Clippy')][string[]]$Check = @('Terminals', 'Clippy'),
    [switch]$Offline
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
if (!$AppBinary) { $AppBinary = Join-Path $repo 'src-tauri\target\release\local-voice-input.exe' }
if (!$ManagementCredentialPath) { $ManagementCredentialPath = Join-Path $repo '.ai\ASR-Eval-local-admin.credential.xml' }
$output = Join-Path $repo ('.ai\evaluation\wave-a-checks-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $output | Out-Null
$hostShell = (Get-Command pwsh.exe -ErrorAction Stop).Source
$statuses = @()
if ('Terminals' -in $Check) {
    $revision = git -C $repo rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Could not identify the source revision.' }
    $terminalArgs = @('-AppBinary', $AppBinary, '-AppRevision', $revision,
        '-ManagementCredentialPath', $ManagementCredentialPath, '-OutputDirectory', (Join-Path $output 'terminals'),
        '-ClassicTerminals', '-MockOnRealProfile')
    if (Test-Path -LiteralPath $AppBinary -PathType Leaf) {
        [ordered]@{
            binary_path = (Resolve-Path -LiteralPath $AppBinary).Path
            binary_sha256 = (Get-FileHash -LiteralPath $AppBinary).Hash
            checkout_base_revision = $revision
            checkout_dirty = [bool](git -C $repo status --porcelain)
            provenance = 'Selected executable, identified by SHA-256. Clippy stages the checkout independently.'
        } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'terminal-candidate.json') -Encoding UTF8
    }
    & $hostShell -NoProfile -File (Join-Path $PSScriptRoot 'run_vm_insertion_smoke.ps1') @terminalArgs
    $statuses += $LASTEXITCODE
}
if ('Clippy' -in $Check) {
    $clippyOutput = Join-Path $output 'clippy'
    $clippyArgs = @('-ManagementCredentialPath', $ManagementCredentialPath, '-OutputDirectory', $clippyOutput)
    if ($Offline) { $clippyArgs += '-Offline' }
    & $hostShell -NoProfile -File (Join-Path $PSScriptRoot 'run_vm_clippy.ps1') @clippyArgs
    $statuses += $LASTEXITCODE
    $resultPath = Join-Path $clippyOutput 'result.json'
    $result = if (Test-Path -LiteralPath $resultPath -PathType Leaf) {
        Get-Content -LiteralPath $resultPath -Raw | ConvertFrom-Json
    }
    if ($result.status -eq 'PASS') {
        $microphoneScript = Join-Path $result.guest_source 'scripts\check_vm_microphone_hotplug.ps1'
        Write-Host 'In the ASR-Eval desktop, check the microphone with:'
        Write-Host ('powershell.exe -NoProfile -ExecutionPolicy Bypass -File "{0}" -List' -f $microphoneScript)
        Write-Host 'Omit -List to start the physical test when one USB recording endpoint is listed.'
    }
}
Write-Host "Evidence $output"
if (1 -in $statuses) { exit 1 }
if (@($statuses | Where-Object { $_ -ne 0 }).Count) { exit 2 }
exit 0
