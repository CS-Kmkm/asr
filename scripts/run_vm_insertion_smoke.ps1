param(
    [Parameter(Mandatory)][string]$AppBinary,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string]$AppRevision,
    [Parameter(Mandatory)][string]$ManagementCredentialPath,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [switch]$IncludeNotepad,
    [switch]$ExpandedMatrix,
    [switch]$ClassicTerminals,
    [ValidateSet('full', 'pilot')][string]$NormalMatrix,
    [switch]$InflightGuards,
    [switch]$RealModelNotepad,
    [switch]$RealApps,
    [switch]$NativeNotepad,
    [ValidateSet('qwen3-asr-0.6b', 'haas-ja-v1.0')][string]$NativeModel = 'qwen3-asr-0.6b',
    [string]$NativeCacheDirectory,
    [string]$NativeCandidatePath,
    [switch]$MockOnRealProfile,
    [ValidateRange(1, 80)][int]$RealRepeats = 1,
    [ValidateSet('original', 'short')][string]$RealFixture = 'original',
    [string]$ManifestDirectory,
    [string]$CancellationPath
)

$ErrorActionPreference = 'Stop'
$referenceRun = $RealModelNotepad -or $NativeNotepad
if ($NativeNotepad -and ($RealModelNotepad -or $RealApps -or $IncludeNotepad -or $ExpandedMatrix -or
        $NormalMatrix -or $ClassicTerminals -or $InflightGuards -or $MockOnRealProfile -or $RealFixture -ne 'short')) {
    throw 'Native Notepad requires the short fixture and its own evaluation suite.'
}
if ([bool]$NativeCacheDirectory -ne $NativeNotepad.IsPresent -or [bool]$NativeCandidatePath -ne $NativeNotepad.IsPresent) {
    throw 'Native Notepad requires the isolated build manifest and an explicit pinned cache directory.'
}
if ($PSBoundParameters.ContainsKey('NativeModel') -and -not $NativeNotepad) { throw 'NativeModel requires NativeNotepad.' }
if ($RealApps -and -not $RealModelNotepad) { throw 'RealApps requires RealModelNotepad.' }
if ($IncludeNotepad -and $RealModelNotepad) { throw 'Choose one Notepad mode.' }
if ($ExpandedMatrix -and ($IncludeNotepad -or $RealModelNotepad)) { throw 'Choose one fixture suite.' }
if ($ClassicTerminals -and ($ExpandedMatrix -or $IncludeNotepad -or $RealModelNotepad)) { throw 'Choose one fixture suite.' }
if ($NormalMatrix -and ($ClassicTerminals -or $ExpandedMatrix -or $IncludeNotepad -or $RealModelNotepad)) { throw 'Choose one fixture suite.' }
if ($InflightGuards -and ($NormalMatrix -or $ClassicTerminals -or $ExpandedMatrix -or $IncludeNotepad -or $RealModelNotepad)) { throw 'Choose one fixture suite.' }
if ($MockOnRealProfile -and $RealModelNotepad) { throw 'MockOnRealProfile is a mock-only mode.' }
if (($RealRepeats -ne 1 -or $RealFixture -ne 'original') -and -not $referenceRun) {
    throw 'RealRepeats and RealFixture require a real model suite.'
}
$expectedUser = if ($referenceRun -or $MockOnRealProfile) { 'ASREvalReal' } else { 'ASREval' }
$repo = Split-Path $PSScriptRoot -Parent
if (-not $ManifestDirectory) { $ManifestDirectory = Join-Path $repo 'bench_data\manifests' }
$output = (New-Item -ItemType Directory -Path $OutputDirectory).FullName
$runId = [guid]::NewGuid().ToString('N')
$taskName = 'ASR-Eval-InsertionSmoke'
$report = [ordered]@{ schema = 'asr-vm-insertion-host/1'; status = 'BLOCKED' }
$session = $null
$guestRoot = $null
$controllerMutex = [Threading.Mutex]::new($false, 'Global\ASR-Eval-InsertionSmoke-Controller')
$locked = $false
function Assert-NotCancelled {
    if ($CancellationPath -and (Test-Path -LiteralPath $CancellationPath -PathType Leaf)) {
        throw 'The evaluation was cancelled by the host controller.'
    }
}
try {
    try { $locked = $controllerMutex.WaitOne(0) }
    catch [Threading.AbandonedMutexException] { $locked = $true }
    if (-not $locked) { throw 'Another host controller still owns ASR-Eval.' }
    Assert-NotCancelled
    $report.host_script_sha256 = (Get-FileHash -LiteralPath $PSCommandPath).Hash
    $vm = Get-VM -Name 'ASR-Eval'
    $report.vm_id = [string]$vm.Id
    if ($vm.State -ne 'Running') { throw 'ASR-Eval must be running with its test user signed in.' }
    $credential = Import-Clixml -LiteralPath $ManagementCredentialPath
    $app = Get-Item -LiteralPath $AppBinary
    $digest = (Get-FileHash -LiteralPath $app.FullName).Hash
    if ($NativeNotepad) {
        $candidate = Get-Content -Raw -Encoding UTF8 -LiteralPath $NativeCandidatePath | ConvertFrom-Json
        $variant = @{
            'qwen3-asr-0.6b' = @{ config = 'tauri.native-evaluation.conf.json'; identifier = 'org.localvoice.input.eval-native'; file = 'Qwen3-ASR-0.6B-Q8_0.gguf' }
            'haas-ja-v1.0' = @{ config = 'tauri.haas-evaluation.conf.json'; identifier = 'org.localvoice.input.eval-haas'; file = 'Haas-616M-ja-v1.0-Q8_0.gguf' }
        }[$NativeModel]
        $config = Join-Path $PSScriptRoot $variant.config
        $identifier = (Get-Content -Raw -LiteralPath $config | ConvertFrom-Json).identifier
        if ($identifier -ne $variant.identifier -or
                $candidate.identifier -ne $identifier -or $candidate.revision -ne $AppRevision -or
                $candidate.app_sha256 -ne $digest -or $candidate.config_sha256 -ne (Get-FileHash -LiteralPath $config).Hash -or
                -not [Text.Encoding]::Latin1.GetString([IO.File]::ReadAllBytes($app.FullName)).Contains($identifier, [StringComparison]::Ordinal)) {
            throw 'Candidate does not match the isolated native evaluation build.'
        }
        $report.native_evaluation_identifier = $identifier
    }
    $session = New-PSSession -VMName $vm.Name -Credential $credential
    if ($ClassicTerminals) {
        $report.classic_tools = @(Invoke-Command -Session $session -ScriptBlock {
            foreach ($tool in 'GitPortable\usr\bin\mintty.exe', 'GitPortable\usr\bin\bash.exe',
                'GitPortable\usr\bin\stty.exe', 'GitPortable\usr\bin\cat.exe', 'GitPortable\usr\bin\timeout.exe',
                'GitPortable\usr\bin\msys-2.0.dll', 'PuTTY\putty.exe') {
                $path = Join-Path 'C:\ASR-Evaluation\tools' $tool
                if (!(Test-Path -LiteralPath $path -PathType Leaf)) { throw "Guest terminal is missing: $tool" }
                [pscustomobject]@{ file = $tool; sha256 = (Get-FileHash -LiteralPath $path).Hash }
            }
        } | Select-Object file,sha256)
    }
    $guestRoot = Invoke-Command -Session $session -ArgumentList $runId,$expectedUser -ScriptBlock {
        param($id, $userName)
        $ErrorActionPreference = 'Stop'
        if ($env:COMPUTERNAME -ne 'ASR-EVAL' -or (Get-CimInstance Win32_ComputerSystem).UserName -ne "ASR-EVAL\$userName") {
            throw 'The dedicated evaluation user is not signed into the expected guest.'
        }
        $user = Get-LocalUser -Name $userName
        $adminGroup = Get-LocalGroup -SID 'S-1-5-32-544'
        if (Get-LocalGroupMember -Group $adminGroup.Name | Where-Object SID -EQ $user.SID) {
            throw 'The evaluation account must be a standard user.'
        }
        $profile = Get-CimInstance Win32_UserProfile -Filter "SID='$($user.SID)'"
        if (-not $profile.Loaded) { throw 'The dedicated evaluation profile is unavailable.' }
        $root = Join-Path $profile.LocalPath "AppData\Local\ASR-Evaluation\$id"
        New-Item -ItemType Directory -Path (Join-Path $root 'scripts') -Force | Out-Null
        New-Item -ItemType Directory -Path (Join-Path $root 'asr_worker') | Out-Null
        $root
    }
    Copy-Item -LiteralPath $app.FullName -Destination "$guestRoot\local-voice-input.exe" -ToSession $session
    $files = 'vm_insertion_smoke.py', 'windows_acceptance.py', 'bench_e2e.py', 'evaluation_events.py', 'score_asr.py', 'windows_uia_bridge.ps1',
        'windows_acceptance_target.html', 'windows_acceptance_terminal.py',
        'windows_acceptance_mintty.sh', 'windows_acceptance_putty.py'
    foreach ($file in $files) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $file) -Destination "$guestRoot\scripts" -ToSession $session
    }
    foreach ($file in Get-ChildItem -LiteralPath (Join-Path $repo 'asr_worker') -File -Filter '*.py') {
        Copy-Item -LiteralPath $file.FullName -Destination "$guestRoot\asr_worker" -ToSession $session
    }
    if ($referenceRun) {
        $report.requested_trials = $RealRepeats
        $manifestName = 'practical-ja-smoke-v1'
        $itemId = 'fleurs-ja_jp-test-1100824364670689611'
        $audioHash = 'a43f19e6accacc590669abca2c0661f5fe3ff71a2cd07344e11d8e3b880d7fe2'
        $referenceHash = 'dc1a6faedb965d307c2a9bae44e28f0382ec4642cd76fb3025c13dd091e44f68'
        if ($RealFixture -eq 'short') {
            $manifestName = 'practical-ja-vm-latency-v1'
            $lock = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $repo "docs\eval_set\public\$manifestName.lock.json") | ConvertFrom-Json
            if ($lock.name -ne $manifestName -or @($lock.items).Count -ne 1) { throw 'Invalid short fixture lock.' }
            $itemId = $lock.items[0].id
            $audioHash = $lock.items[0].audio_sha256
            $referenceHash = $lock.items[0].reference_sha256
        }
        $manifestPath = Join-Path $ManifestDirectory "$manifestName.json"
        $manifest = Get-Content -Raw -Encoding UTF8 -LiteralPath $manifestPath | ConvertFrom-Json
        $items = @($manifest.items | Where-Object id -EQ $itemId)
        if ($items.Count -ne 1) { throw 'Missing or duplicate pinned evaluation utterance.' }
        $item = $items[0]
        $audio = Join-Path (Split-Path $manifestPath -Parent) $item.audio_path
        $sha = [Security.Cryptography.SHA256]::Create()
        try { $actualReferenceHash = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($item.reference))).Replace('-', '') }
        finally { $sha.Dispose() }
        if ($item.audio_sha256 -ne $audioHash -or (Get-FileHash -LiteralPath $audio).Hash -ne $audioHash -or
            $actualReferenceHash -ne $referenceHash -or $item.language -notlike 'ja*') {
            throw 'Pinned audio or reference hash mismatch.'
        }
        $item.audio_path = "$($item.id).wav"
        $manifest.items = @($item)
        $fixtureJson = $manifest | ConvertTo-Json -Depth 16 -Compress
        Invoke-Command -Session $session -ArgumentList $guestRoot,$fixtureJson -ScriptBlock {
            param($root, $json)
            $fixtureRoot = New-Item -ItemType Directory -Path (Join-Path $root 'real-fixture')
            [IO.File]::WriteAllText((Join-Path $fixtureRoot.FullName 'manifest.json'), $json, [Text.UTF8Encoding]::new($false))
        }
        Copy-Item -LiteralPath $audio -Destination "$guestRoot\real-fixture\$($item.audio_path)" -ToSession $session
        $report.input_audio_sha256 = $audioHash
        $report.input_reference_sha256 = $referenceHash
        $report.input_item_id = $itemId
        $report.fixture = $RealFixture
        if ($NativeNotepad) {
            $catalog = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $repo 'src\asr-model-catalog.json') | ConvertFrom-Json
            $model = @($catalog.models | Where-Object id -EQ "speech-cpp:${NativeModel}")
            if ($model.Count -ne 1 -or $model[0].modelId -ne $NativeModel -or $model[0].weightFormat -ne 'Q8_0' -or
                    $model[0].source.revision -notmatch '^[a-f0-9]{40}$' -or
                    $model[0].source.file -ne $variant.file -or
                    $model[0].source.sha256 -notmatch '^[a-f0-9]{64}$') { throw 'Invalid native model catalog pin.' }
            $source = $model[0].source
            $report.model_revision = $source.revision
            $report.native_model = $model[0].modelId
            $report.native_device = 'cpu'
            $expected = [ordered]@{
                'runtime-0.10.1\speech.exe' = '46f264e26fcaa60f433455c6c6092137c123af67fad956d67f250daf5ebaa893'
                'runtime-0.10.1\speech.dll' = '923fb0babff5596202ae92dc5b6423a23579ab461bc7cce8b35258107ee05662'
            }
            $expected[(Join-Path $source.revision $source.file)] = $source.sha256
            $report.model_files_sha256 = @(foreach ($file in $expected.Keys) {
                $localAsset = Join-Path $NativeCacheDirectory $file
                if ((Get-FileHash -LiteralPath $localAsset).Hash -ne $expected[$file]) { throw "Native asset hash mismatch: $file" }
                $guestAsset = Join-Path "$guestRoot\native-cache" $file
                Invoke-Command -Session $session -ArgumentList $guestAsset -ScriptBlock {
                    param($path)
                    New-Item -ItemType Directory -Path (Split-Path $path -Parent) -Force | Out-Null
                }
                Copy-Item -LiteralPath $localAsset -Destination $guestAsset -ToSession $session
                $actual = Invoke-Command -Session $session -ArgumentList $guestAsset -ScriptBlock {
                    param($path)
                    (Get-FileHash -LiteralPath $path).Hash
                }
                if ($actual -ne $expected[$file]) { throw "Staged native asset hash mismatch: $file" }
                [pscustomobject]@{ file = $file; sha256 = $actual }
            })
        } else {
            $report.model_revision = '0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf'
            $report.model_files_sha256 = Invoke-Command -Session $session -ArgumentList $report.model_revision -ScriptBlock {
            param($revision)
            $modelDir = "C:\ASR-Evaluation\models\faster-whisper-$revision"
            $expected = [ordered]@{
                'config.json' = 'B0253EA6C0D3BEA6B1E19E91A02ACFD3B53F4467362EFCB5A3E6B16C9B3A9B7E'
                'model.bin' = 'E76620F83D5F5B69EFD3D87E3DC180C1BD21DF9FBEBACFD4335E5E1EFCC018DA'
                'preprocessor_config.json' = '7CCC62C6F2765AF1F3B46C00C9B5894426835A05021C8B9C01EECB6DFB542711'
                'tokenizer.json' = '297B13372AC43916285644FB9687ADD3CC62EE2A1ADB60DA3DC25CC94C1871FD'
                'vocabulary.json' = 'C69260F2AB26D659B7C398F9A2B2B48ED0DF16C3B47D7326782FD9CBA71690C1'
            }
            foreach ($file in $expected.Keys) {
                if ((Get-FileHash -LiteralPath (Join-Path $modelDir $file)).Hash -ne $expected[$file]) {
                    throw "Guest model file hash mismatch: $file"
                }
                [pscustomobject]@{ file = $file; sha256 = $expected[$file] }
            }
            } | Select-Object file,sha256
        }
    }
    Assert-NotCancelled
    $externalRepeats = 2
    $externalTakes = if ($RealApps) { 2 * $externalRepeats } else { 0 }
    $limit = if ($NormalMatrix -eq 'full') { 68400 } elseif ($NormalMatrix -eq 'pilot') { 1800 } elseif ($referenceRun) { 155 + 160 * ($RealRepeats + $externalTakes) } elseif ($ExpandedMatrix) { 1100 } elseif ($InflightGuards) { 480 } elseif ($IncludeNotepad) { 380 } else { 290 }
    $previousRun = Invoke-Command -Session $session -ArgumentList $guestRoot,$taskName,$digest,$AppRevision,$runId,($IncludeNotepad.IsPresent),($RealModelNotepad.IsPresent),$expectedUser,$RealRepeats,$limit,($MockOnRealProfile.IsPresent),($ExpandedMatrix.IsPresent),($RealApps.IsPresent),($ClassicTerminals.IsPresent),$NormalMatrix,($InflightGuards.IsPresent),($NativeNotepad.IsPresent),$NativeModel -ScriptBlock {
        param($root, $task, $expectedDigest, $revision, $owner, $notepad, $real, $userName, $repeats, $limit, $sharedMock, $expanded, $realApps, $classicTerminals, $normalMatrix, $inflightGuards, $nativeNotepad, $nativeModel)
        $ErrorActionPreference = 'Stop'
        if ((Get-FileHash -LiteralPath (Join-Path $root 'local-voice-input.exe')).Hash -ne $expectedDigest) {
            throw 'The staged guest executable has an unexpected hash.'
        }
        $uv = 'C:\ASR-Evaluation\runtime\uv.exe'
        $python = 'C:\ASR-Evaluation\runtime\python\python.exe'
        if (-not (Test-Path -LiteralPath $uv) -or -not (Test-Path -LiteralPath $python)) {
            throw 'The provisioned guest uv or Python runtime is missing.'
        }
        $command = @'
$ErrorActionPreference = 'Stop'
$resultPath = Join-Path $PWD 'runner.log'
$entry = Join-Path $PWD 'scripts\vm_insertion_smoke.py'
& 'C:\ASR-Evaluation\runtime\uv.exe' run --offline --no-project --python 'C:\ASR-Evaluation\runtime\python\python.exe' $entry --revision __REVISION__ __EXTERNAL_CASE__ *> $resultPath
exit $LASTEXITCODE
'@
        $externalCase = if ($nativeNotepad) { "--native-notepad --native-model $nativeModel --real-repeats $repeats" } elseif ($realApps) { "--real-apps --real-repeats $repeats" } elseif ($real) { "--real-notepad --real-repeats $repeats" } elseif ($expanded) {
            '--expanded-matrix'
        } elseif ($classicTerminals) { '--classic-terminals' } elseif ($normalMatrix -eq 'full') { '--normal-matrix' } elseif ($normalMatrix -eq 'pilot') { '--normal-matrix-pilot' } elseif ($inflightGuards) { '--inflight-guards' } elseif ($notepad) { '--include-notepad' } else { '' }
        if ($sharedMock) { $externalCase += ' --mock-on-real-profile' }
        $command = $command.Replace('__REVISION__', $revision).Replace('__EXTERNAL_CASE__', $externalCase)
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
        $pwsh = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $action = New-ScheduledTaskAction -Execute $pwsh -Argument "-NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand $encoded" -WorkingDirectory $root
        $principal = New-ScheduledTaskPrincipal -UserId "ASR-EVAL\$userName" -LogonType Interactive -RunLevel Limited
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds $limit)
        Register-ScheduledTask -TaskName $task -Description $owner -Action $action -Principal $principal -Settings $settings | Out-Null
        $previousRun = (Get-ScheduledTaskInfo -TaskName $task).LastRunTime
        Start-ScheduledTask -TaskName $task
        $previousRun
    }
    $deadline = (Get-Date).AddSeconds($limit)
    $pollMilliseconds = if ($NormalMatrix -eq 'full') { 5000 } else { 750 }
    do {
        Assert-NotCancelled
        Start-Sleep -Milliseconds $pollMilliseconds
        $completed = Invoke-Command -Session $session -ArgumentList $taskName,$runId,$previousRun -ScriptBlock {
            param($task, $owner, $previous)
            $registered = Get-ScheduledTask -TaskName $task
            if ($registered.Description -ne $owner) { throw 'Guest task ownership changed.' }
            $registered.State -eq 'Ready' -and (Get-ScheduledTaskInfo -TaskName $task).LastRunTime -ne $previous
        }
    } while (-not $completed -and (Get-Date) -lt $deadline)
    Assert-NotCancelled
    foreach ($file in 'result.json', 'runner.log') {
        if (Invoke-Command -Session $session -ArgumentList "$guestRoot\$file" -ScriptBlock { param($path) Test-Path -LiteralPath $path -PathType Leaf }) {
            Copy-Item -FromSession $session -LiteralPath "$guestRoot\$file" -Destination $output
        }
    }
    $casesPath = "$guestRoot\cases"
    if (Invoke-Command -Session $session -ArgumentList $casesPath -ScriptBlock { param($path) Test-Path -LiteralPath $path -PathType Container }) {
        Copy-Item -FromSession $session -LiteralPath $casesPath -Destination $output -Recurse
    }
    if (-not $completed) { throw 'The guest task did not finish before the deadline.' }
    $guestResult = Get-Content -LiteralPath (Join-Path $output 'result.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($guestResult.schema -ne 'asr-vm-insertion-smoke/2' -or $guestResult.app_source_revision -ne $AppRevision -or $guestResult.exe_sha256 -ne $digest) {
        throw 'Guest report identity could not be verified.'
    }
    if ($guestResult.mock_on_real_profile -ne $MockOnRealProfile.IsPresent) {
        throw 'Guest reported an unexpected staged mock worker mode.'
    }
    $caseNames = $guestResult.cases.scenario -join ','
    $expectedCases = 'mock-insertion,focus-away,cancel'
    if ($IncludeNotepad) { $expectedCases += ',mock-notepad' }
    if ($ExpandedMatrix) { $expectedCases += ',selection-change,minimize,mock-notepad,mock-edge,mock-vscode,mock-chrome,mock-terminal,worker-crash-recovery' }
    if ($RealModelNotepad) { $expectedCases = 'real-notepad' }
    if ($NativeNotepad) { $expectedCases = 'native-notepad' }
    if ($RealApps) { $expectedCases += ',real-vscode,real-chrome' }
    if ($ClassicTerminals) { $expectedCases = 'mock-mintty,mock-putty' }
    if ($NormalMatrix) { $expectedCases = 'mock-insertion,mock-notepad,mock-chrome,mock-vscode,mock-terminal' }
    if ($InflightGuards) { $expectedCases = 'ime-composition,cancel-processing,clipboard-rich,clipboard-newer-during-asr' }
    if ($caseNames -ne $expectedCases -or @($guestResult.cases | Where-Object status -NotIn @('PASS','FAIL','BLOCKED')).Count) {
        throw 'Guest case coverage could not be verified.'
    }
    if ($referenceRun) {
        foreach ($realCase in $guestResult.cases | Where-Object status -EQ 'PASS') {
            $count = if ($realCase.scenario -in @('real-notepad', 'native-notepad')) { $RealRepeats } else { $externalRepeats }
            if ($count -gt 1 -and (
                $realCase.requested_trials -ne $count -or @($realCase.trials).Count -ne $count -or
                ($realCase.trials.index -join ',') -ne ((1..$count) -join ',') -or
                @($realCase.trials | Where-Object status -NE 'PASS').Count)) {
                throw 'Guest real trial coverage could not be verified.'
            }
        }
    }
    if ($NativeNotepad -and $guestResult.status -eq 'PASS') {
        $nativeCase = $guestResult.cases[0]
        $nativeTrials = if ($RealRepeats -gt 1) { $nativeCase.trials } else { @($nativeCase) }
        $first = $nativeTrials[0]
        if ($nativeCase.native_model -ne $NativeModel -or $nativeCase.native_worker_reaped -ne $true -or
                -not $first.native_worker_pid -or -not $first.native_worker_started -or
                -not $first.application_pid -or @($nativeTrials | Where-Object {
                    $_.native_worker_pid -ne $first.native_worker_pid -or $_.native_worker_started -ne $first.native_worker_started -or
                    $_.application_pid -ne $first.application_pid -or -not $_.acceptance.results[0].stop_to_observed_insertion_ms -or
                    $_.acceptance.results[0].reference_normalized_cer -gt 0.5
                }).Count) {
            throw 'Guest native CPU trial identity or independent receiver evidence could not be verified.'
        }
    }
    if ($ExpandedMatrix) {
        $recovery = $guestResult.cases | Where-Object scenario -EQ 'worker-crash-recovery'
        if ($recovery.status -eq 'PASS') {
            $trials = @($recovery.trials)
            if ($recovery.requested_trials -ne 2 -or $trials.Count -ne 2 -or
                ($trials.index -join ',') -ne '1,2' -or ($trials.status -join ',') -ne 'PASS,PASS' -or
                ($trials.scenario -join ',') -ne 'worker-crash,mock-insertion' -or
                $trials[0].acceptance.results[0].worker_exit_before_recovery -ne $true -or
                $trials[1].recovered_worker_live -ne $true -or
                -not $trials[0].application_pid -or $trials[0].application_pid -ne $trials[1].application_pid -or
                ($trials[0].capture_proof.worker_pid -eq $trials[1].capture_proof.worker_pid -and
                 $trials[0].capture_proof.worker_start_time -eq $trials[1].capture_proof.worker_start_time)) {
                throw 'Guest worker recovery evidence could not be verified.'
            }
        }
    }
    if ($InflightGuards) {
        foreach ($case in $guestResult.cases | Where-Object status -EQ 'PASS') {
            $row = $case.acceptance.results[0]
            if ($case.capture_proof.passed -ne $true -or $case.transcription_requests -ne 1 -or
                    ($case.scenario -in @('ime-composition','cancel-processing') -and $row.receiver_unchanged -ne $true) -or
                    ($case.scenario -eq 'cancel-processing' -and ($row.processing_cancel_attempted -ne $true -or
                        $row.worker_hold_observed -ne $true -or $row.app_outcome -ne 'cancelled')) -or
                    ($case.scenario -eq 'ime-composition' -and -not $row.verification_scope) -or
                    ($case.scenario -like 'clipboard-*' -and ($row.clipboard_preserved -ne $true -or $row.exact_receiver_match -ne $true)) -or
                    ($case.scenario -eq 'clipboard-newer-during-asr' -and $row.worker_hold_observed -ne $true)) {
                throw 'Guest in-flight guard evidence could not be verified.'
            }
        }
    }
    $caseStatuses = @($guestResult.cases.status)
    if ($NormalMatrix) {
        $samples = if ($NormalMatrix -eq 'full') { 50 } else { 1 }
        $conditions = 'short', 'mixed', 'multiline', 'long'
        $planned = 20 * $samples
        $reported = 0
        $confirmed = 0
        $blocked = 0
        $nonces = [Collections.Generic.HashSet[string]]::new()
        foreach ($case in $guestResult.cases) {
            $nextIndex = 2
            if ($case.warmup -and ($case.warmup.index -ne 1 -or $case.warmup.condition -ne 'short' -or
                    $case.warmup.nonce -cnotmatch '^[a-f0-9]{16}$' -or -not $nonces.Add($case.warmup.nonce))) {
                throw 'Guest matrix warm-up identity could not be verified.'
            }
            foreach ($trial in @($case.trials | Where-Object { $_ })) {
                if ($trial.index -ne $nextIndex -or $trial.condition -ne $conditions[($nextIndex - 2) % 4] -or
                        $trial.scenario -ne $case.scenario -or $trial.nonce -cnotmatch '^[a-f0-9]{16}$' -or
                        -not $nonces.Add($trial.nonce) -or $trial.status -notin @('PASS', 'FAIL', 'BLOCKED')) {
                    throw 'Guest matrix trial identity could not be verified.'
                }
                if ($trial.status -eq 'PASS') {
                    $row = $trial.acceptance.results[0]
                    if ($row.exact_receiver_match -ne $true -or $trial.capture_proof.matrix_nonce -ne $trial.nonce -or
                            $trial.capture_proof.effective_backend -ne 'mock' -or $trial.capture_proof.passed -ne $true -or
                            $trial.transcription_requests -ne 1) {
                        throw 'Guest matrix receiver and capture evidence could not be verified.'
                    }
                    $confirmed++
                }
                if ($trial.status -eq 'BLOCKED') { $blocked++ }
                $reported++
                $nextIndex++
            }
            if ($nextIndex -gt (4 * $samples + 2) -or ($nextIndex -gt 2 -and -not $case.warmup)) {
                throw 'Guest matrix target coverage could not be verified.'
            }
        }
        $matrix = $guestResult.normal_matrix
        if (-not $matrix -or $matrix.planned -ne $planned -or $matrix.reported -ne $reported -or
                $matrix.confirmed -ne $confirmed -or $matrix.blocked -ne $blocked) {
            throw 'Guest matrix totals could not be verified.'
        }
        $report.normal_matrix = $matrix
        $expectedStatus = if ($reported -ne $planned -or $blocked -eq $planned -or $guestResult.requires_session_reset) { 'BLOCKED' } elseif (200 * $confirmed -ge 199 * $planned) { 'PASS' } else { 'FAIL' }
    } else {
        $expectedStatus = if ($caseStatuses -contains 'FAIL') { 'FAIL' } elseif ($caseStatuses -contains 'BLOCKED') { 'BLOCKED' } else { 'PASS' }
    }
    if ($guestResult.status -ne $expectedStatus) { throw 'Guest suite and case statuses disagree.' }
    $report.status = $expectedStatus
    $report.cases = @($guestResult.cases | Select-Object scenario,status,reason)
    if ($report.status -ne 'PASS') {
        $report.reason = $guestResult.reason
        if (-not $report.reason) { $report.reason = ($report.cases | Where-Object status -NE 'PASS' | Select-Object -First 1).reason }
    }
} catch {
    $report.reason = $_.Exception.Message
} finally {
    if ($session) {
        try {
            Invoke-Command -Session $session -ArgumentList $taskName,$runId,$guestRoot,$expectedUser -ScriptBlock {
                param($task, $owner, $root, $userName)
                $registered = Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
                if ($registered.Description -eq $owner) {
                    $forced = $registered.State -in @('Running', 'Queued')
                    if ($forced) {
                        Stop-ScheduledTask -TaskName $task
                    }
                    $appPath = Join-Path $root 'local-voice-input.exe'
                    $entry = Join-Path $root 'scripts\vm_insertion_smoke.py'
                    $userSid = (Get-LocalUser -Name $userName).SID.Value
                    $fixtureProfiles = Join-Path $root 'target-profiles'
                    $terminalEntry = Join-Path $root 'scripts\windows_acceptance_terminal.py'
                    $puttyEntry = Join-Path $root 'scripts\windows_acceptance_putty.py'
                    $nativeWorker = Join-Path $root 'native-cache\runtime-0.10.1\speech.exe'
                    $findRunProcesses = {
                        Get-CimInstance Win32_Process -Filter "Name='local-voice-input.exe' OR Name='python.exe' OR Name='uv.exe' OR Name='speech.exe'" |
                            Where-Object {
                                $_.ExecutablePath -eq $appPath -or (
                                    $_.ExecutablePath -eq $nativeWorker -and
                                    (Invoke-CimMethod -InputObject $_ -MethodName GetOwnerSid -ErrorAction SilentlyContinue).Sid -eq $userSid) -or (
                                    $_.CommandLine -and (
                                        $_.CommandLine.IndexOf($entry, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
                                        $_.CommandLine.IndexOf($terminalEntry, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
                                        $_.CommandLine.IndexOf($puttyEntry, [StringComparison]::OrdinalIgnoreCase) -ge 0))
                            }
                        # Packaged Notepad can outlive the controller's job.
                        # Its command line contains the exact fresh scratch path.
                        $fixtures = @(Get-ChildItem -LiteralPath (Join-Path $root 'cases') -Recurse -File -Filter 'asr-fixture-*.txt' -ErrorAction SilentlyContinue |
                            Where-Object { $_.Name -match '^asr-fixture-[0-9a-f]{32}\.txt$' })
                        if ($fixtures.Count) {
                            Get-CimInstance Win32_Process -Filter "Name='notepad.exe'" | ForEach-Object {
                                $candidate = $_
                                $fixture = $fixtures | Where-Object {
                                    $candidate.CommandLine -match ('(?:^|[\s"])' + [regex]::Escape($_.FullName) + '(?:$|[\s"])')
                                } | Select-Object -First 1
                                if ($fixture -and (Invoke-CimMethod -InputObject $candidate -MethodName GetOwnerSid -ErrorAction SilentlyContinue).Sid -eq $userSid) {
                                    $candidate
                                }
                            }
                        }
                        foreach ($folder in Get-ChildItem -LiteralPath $fixtureProfiles -Directory -ErrorAction SilentlyContinue) {
                            if ($folder.Name -notmatch '^asr-(edge|chrome|code|mintty|putty)-[0-9a-f]{32}$') { continue }
                            $ownerPath = Join-Path $folder.FullName 'window-owner.json'
                            if (-not (Test-Path -LiteralPath $ownerPath -PathType Leaf)) { continue }
                            $window = Get-Content -Raw -LiteralPath $ownerPath | ConvertFrom-Json
                            $processName = @{ edge = 'msedge'; chrome = 'chrome'; code = 'Code'
                                mintty = 'mintty'; putty = 'putty' }[$folder.Name.Split('-')[1]]
                            if ($window.name -ne $processName -or
                                $window.pid -notmatch '^\d+$' -or $window.start_ticks -notmatch '^\d+$') {
                                throw 'invalid external fixture ownership evidence'
                            }
                            $candidate = Get-Process -Id $window.pid -ErrorAction SilentlyContinue
                            if ($candidate -and $candidate.ProcessName -eq $window.name -and
                                $candidate.StartTime.ToUniversalTime() -ge $folder.CreationTimeUtc -and
                                "$($candidate.StartTime.ToUniversalTime().Ticks)" -eq $window.start_ticks) {
                                $cim = Get-CimInstance Win32_Process -Filter "ProcessId=$($candidate.Id)"
                                $marker = if ($processName -in @('mintty', 'putty')) { $folder.Name } else { $folder.FullName }
                                if ($cim -and $cim.CommandLine -and
                                    $cim.CommandLine.IndexOf($marker, [StringComparison]::OrdinalIgnoreCase) -ge 0 -and
                                    (Invoke-CimMethod -InputObject $cim -MethodName GetOwnerSid -ErrorAction SilentlyContinue).Sid -eq $userSid) {
                                    $cim
                                }
                            }
                        }
                    }
                    $settle = [Diagnostics.Stopwatch]::StartNew()
                    $quietAt = $null
                    do {
                        $remaining = @(& $findRunProcesses)
                        if ($remaining.Count) {
                            $quietAt = $null
                            foreach ($process in $remaining) {
                                Invoke-CimMethod -InputObject $process -MethodName Terminate -ErrorAction SilentlyContinue | Out-Null
                            }
                        } else {
                            if ($null -eq $quietAt) { $quietAt = $settle.Elapsed.TotalSeconds }
                            if (-not $forced -or $settle.Elapsed.TotalSeconds - $quietAt -ge 3) { break }
                        }
                        Start-Sleep -Milliseconds 200
                    } while ($settle.Elapsed.TotalSeconds -lt 15)
                    if (@(& $findRunProcesses).Count) { throw 'Evaluation processes did not exit; retaining the task reservation.' }
                    foreach ($folder in Get-ChildItem -LiteralPath $fixtureProfiles -Directory -ErrorAction SilentlyContinue) {
                        if ($folder.Name -notmatch '^asr-putty-[0-9a-f]{32}$') { continue }
                        $sessionKey = "Registry::HKEY_USERS\$userSid\Software\SimonTatham\PuTTY\Sessions\$($folder.Name)"
                        if (Test-Path -LiteralPath $sessionKey) {
                            $value = Get-ItemProperty -LiteralPath $sessionKey
                            if ($value.WinTitle -eq $folder.Name -and $value.HostName -eq '127.0.0.1') {
                                Remove-Item -LiteralPath $sessionKey
                            }
                        }
                    }
                    Unregister-ScheduledTask -TaskName $task -Confirm:$false
                }
            }
        } catch {
            $report.status = 'BLOCKED'
            $report.cleanup_error = $_.Exception.Message
        } finally {
            try { Remove-PSSession -Session $session }
            catch {
                $report.status = 'BLOCKED'
                if (-not $report.cleanup_error) { $report.cleanup_error = $_.Exception.Message }
            }
        }
    }
    try { $report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $output 'host.json') -Encoding UTF8 }
    finally {
        if ($locked) { $controllerMutex.ReleaseMutex() }
        $controllerMutex.Dispose()
    }
}

Write-Output "$($report.status) $output"
if ($report.status -eq 'PASS') { exit 0 }
if ($report.status -eq 'FAIL') { exit 1 }
exit 2
