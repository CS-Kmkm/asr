param(
    [Parameter(Mandatory)][string]$ManagementCredentialPath,
    [string]$OutputDirectory,
    [switch]$Offline
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$run = [guid]::NewGuid().ToString('N')
if (!$OutputDirectory) { $OutputDirectory = Join-Path $repo ".ai\evaluation\wave-a-clippy-$run" }
$output = (New-Item -ItemType Directory -Path $OutputDirectory -ErrorAction Stop).FullName
$report = [ordered]@{ schema = 'asr-vm-clippy/1'; status = 'BLOCKED' }
$session = $null
try {
    $vm = Get-VM -Name 'ASR-Eval'
    if ($vm.State -ne 'Running') { throw 'ASR-Eval must be running.' }
    if (!(Test-Path -LiteralPath (Join-Path $repo 'dist\index.html') -PathType Leaf)) {
        throw 'A built frontend is required for Tauri embedding. Build dist before staging.'
    }
    $report.vm_id = [string]$vm.Id
    $manifest = Join-Path $output 'source-files.txt'
    $archive = Join-Path $output 'source.tar'
    Push-Location $repo
    try {
        $report.base_revision = (git rev-parse HEAD).Trim()
        if ($LASTEXITCODE -ne 0) { throw 'Could not identify the workspace revision.' }
        $report.dirty = [bool](git status --porcelain)
        if ($LASTEXITCODE -ne 0) { throw 'Could not inspect workspace modifications.' }
        $paths = @(git ls-files --cached)
        if ($LASTEXITCODE -ne 0) { throw 'Could not enumerate the tracked source.' }
        $untracked = @(git ls-files --others --exclude-standard -- src-tauri/src)
        if ($LASTEXITCODE -ne 0) { throw 'Could not enumerate local Rust modules.' }
        $paths = @($paths + @($untracked | Where-Object { $_ -like '*.rs' }) |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Sort-Object -Unique)
        $paths += @('scripts/check_vm_microphone_hotplug.ps1', 'scripts/vm_wave_a_build_env.ps1') |
            Where-Object { $_ -notin $paths }
        [IO.File]::WriteAllLines($manifest, [string[]]$paths, [Text.UTF8Encoding]::new($false))
        & tar.exe -cf $archive -T $manifest dist
        if ($LASTEXITCODE -ne 0) { throw 'Could not package the workspace for the guest.' }
    } finally { Pop-Location }
    $report.source_sha256 = (Get-FileHash -LiteralPath $archive).Hash
    $session = New-PSSession -VMName $vm.Name -Credential (Import-Clixml -LiteralPath $ManagementCredentialPath)
    $guest = Invoke-Command -Session $session -ArgumentList $run -ScriptBlock {
        param($id)
        if ($env:COMPUTERNAME -ne 'ASR-EVAL') { throw 'Wrong guest.' }
        (New-Item -ItemType Directory -Path "C:\ASR-Evaluation\source\$id").FullName
    }
    $report.guest_source = $guest
    Copy-Item -LiteralPath $archive,(Join-Path $PSScriptRoot 'vm_wave_a_build_env.ps1') -Destination $guest -ToSession $session
    $result = Invoke-Command -Session $session -ArgumentList $guest,$report.source_sha256,$Offline.IsPresent -ScriptBlock {
        param($root, $digest, $offline)
        $ErrorActionPreference = 'Stop'
        if ($env:COMPUTERNAME -ne 'ASR-EVAL' -or (Get-FileHash -LiteralPath "$root\source.tar").Hash -ne $digest) {
            throw 'Guest source identity could not be verified.'
        }
        Set-Location $root
        & tar.exe -xf source.tar
        if ($LASTEXITCODE -ne 0) { throw 'Guest source extraction failed.' }
        . ([scriptblock]::Create([IO.File]::ReadAllText("$root\vm_wave_a_build_env.ps1")))
        $tool = & $waveCargo +1.97.1 clippy --version
        if ($LASTEXITCODE -ne 0) { throw 'Guest Clippy is unavailable.' }
        Set-Location "$root\src-tauri"
        # Capture stderr as well as stdout without treating native diagnostics as PowerShell errors.
        $ErrorActionPreference = 'Continue'
        $fetchCode = 0
        if (!$offline) {
            & $waveCargo +1.97.1 fetch --locked *> "$root\fetch.log"
            $fetchCode = $LASTEXITCODE
        }
        $exitCode = $null
        $testCode = $null
        $testDigest = $null
        if ($fetchCode -eq 0) {
            & $waveCargo +1.97.1 clippy --all-targets --locked --offline -- -D warnings *> "$root\clippy.log"
            $exitCode = $LASTEXITCODE
        }
        if ($exitCode -eq 0 -and $null -ne $exitCode) {
            $artifacts = & $waveCargo +1.97.1 test --lib --locked --offline --no-run --message-format=json 2> "$root\test-build.log"
            $testCode = $LASTEXITCODE
            if ($testCode -eq 0) {
                $binary = @($artifacts | ForEach-Object { $_ | ConvertFrom-Json } |
                    Where-Object { $_.reason -eq 'compiler-artifact' -and $_.profile.test -and $_.executable })
                if ($binary.Count -ne 1) { throw 'The guest microphone test executable could not be identified.' }
                $stagedTest = "$root\physical-microphone-test.exe"
                Copy-Item -LiteralPath $binary[0].executable -Destination $stagedTest
                $env:Path = "C:\ASR-Evaluation\runtime\python;$env:Path"
                $microphoneCase = 'audio::hotplug_test::physical_usb_microphone_disconnect_and_reconnect'
                $listed = & $stagedTest $microphoneCase --exact --list
                if ($LASTEXITCODE -ne 0 -or "$($microphoneCase): test" -notin $listed) {
                    throw 'The staged guest binary does not expose the physical microphone test.'
                }
                $testDigest = (Get-FileHash -LiteralPath $stagedTest).Hash
            }
        }
        [pscustomobject]@{ tool = $tool; computer = $env:COMPUTERNAME; fetch_exit = $fetchCode
            clippy_exit = $exitCode; test_build_exit = $testCode; microphone_test_sha256 = $testDigest }
    }
    foreach ($name in 'fetch.log', 'clippy.log', 'test-build.log') {
        if (Invoke-Command -Session $session -ArgumentList "$guest\$name" -ScriptBlock { param($path) Test-Path -LiteralPath $path }) {
            Copy-Item -FromSession $session -LiteralPath "$guest\$name" -Destination $output
        }
    }
    $report.computer = $result.computer
    $report.tool = $result.tool
    $report.fetch_exit = $result.fetch_exit
    $report.clippy_exit = $result.clippy_exit
    $report.test_build_exit = $result.test_build_exit
    $report.microphone_test_sha256 = $result.microphone_test_sha256
    if ($result.fetch_exit -eq 0) {
        $report.status = if ($result.clippy_exit -eq 0 -and $result.test_build_exit -eq 0) { 'PASS' } else { 'FAIL' }
    } elseif (Select-String -LiteralPath (Join-Path $output 'fetch.log') -Quiet -Pattern 'lock file .*needs to be updated|cannot update the lock file') {
        $report.status = 'FAIL'
    }
} catch {
    $report.reason = $_.Exception.Message
} finally {
    if ($session) { Remove-PSSession $session }
    $report | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'result.json') -Encoding UTF8
}
Write-Host "$($report.status) $output"
switch ($report.status) { 'PASS' { exit 0 }; 'FAIL' { exit 1 }; default { exit 2 } }
