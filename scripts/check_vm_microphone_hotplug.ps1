param(
    [string]$DeviceInstanceId,
    [switch]$List
)

$ErrorActionPreference = 'Stop'
if ($env:COMPUTERNAME -ne 'ASR-EVAL') { throw 'Run this from the ASR-Eval desktop.' }
$recordingEndpoints = @(Get-PnpDevice -PresentOnly -Class AudioEndpoint |
    Where-Object InstanceId -Match '^SWD\\MMDEVAPI\\\{0\.0\.1\.')
$endpoints = @(foreach ($endpoint in $recordingEndpoints) {
    $parent = $endpoint.InstanceId
    for ($depth = 0; $depth -lt 12 -and $parent; $depth++) {
        if ($parent -match '^USB\\VID_[0-9A-F]{4}&PID_[0-9A-F]{4}') {
            [pscustomobject]@{ Name = $endpoint.FriendlyName; InstanceId = $endpoint.InstanceId; UsbAncestor = $parent }
            break
        }
        $parent = (Get-PnpDeviceProperty -InstanceId $parent -KeyName DEVPKEY_Device_Parent -ErrorAction SilentlyContinue).Data
    }
})
if ($List) {
    $endpoints | Format-Table -Wrap
    if (!$endpoints.Count) { Write-Host 'BLOCKED: No physical USB recording endpoint is exposed to the guest.'; exit 2 }
    exit 0
}
$selected = @($endpoints | Where-Object { !$DeviceInstanceId -or $_.InstanceId -eq $DeviceInstanceId })
if ($selected.Count -ne 1 -or @($recordingEndpoints | Where-Object FriendlyName -eq $selected[0].Name).Count -ne 1) {
    $endpoints | Format-Table -Wrap
    Write-Host 'BLOCKED: Select one physical USB recording endpoint with a unique capture name.'
    exit 2
}

$root = Split-Path $PSScriptRoot -Parent
$binary = Join-Path $root 'physical-microphone-test.exe'
if (!(Test-Path -LiteralPath $binary -PathType Leaf)) {
    Write-Host 'BLOCKED: Run the host Clippy runner to build and stage the guest test executable.'
    exit 2
}
$env:ASR_VM_MIC_NAME = $selected[0].Name
$env:ASR_VM_MIC_INSTANCE_ID = $selected[0].InstanceId
$env:Path = "C:\ASR-Evaluation\runtime\python;$env:Path"
$evidence = Join-Path $env:LOCALAPPDATA 'ASR-Evaluation'
New-Item -ItemType Directory -Path $evidence -Force | Out-Null
$run = New-Item -ItemType Directory -Path (Join-Path $evidence ('microphone-hotplug-' + [guid]::NewGuid().ToString('N')))
$log = Join-Path $run.FullName 'capture.log'
"Selected microphone: $($selected[0].Name) [$($selected[0].InstanceId)]" | Tee-Object -FilePath $log
$ErrorActionPreference = 'Continue'
& $binary audio::hotplug_test::physical_usb_microphone_disconnect_and_reconnect --exact --ignored --nocapture 2>&1 |
    Tee-Object -FilePath $log -Append
$exitCode = $LASTEXITCODE
if ($exitCode -eq 0 -and !(Select-String -LiteralPath $log -Quiet -Pattern '^test result: ok\. 1 passed; 0 failed;')) {
    $exitCode = 1
}
[ordered]@{
    schema = 'asr-vm-physical-microphone/1'
    status = if ($exitCode -eq 0) { 'PASS' } else { 'FAIL' }
    name = $selected[0].Name
    endpoint = $selected[0].InstanceId
    usb_ancestor = $selected[0].UsbAncestor
    test_binary_sha256 = (Get-FileHash -LiteralPath $binary).Hash
    exit_code = $exitCode
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $run.FullName 'result.json') -Encoding UTF8
Write-Host "Evidence $($run.FullName)"
exit $exitCode
