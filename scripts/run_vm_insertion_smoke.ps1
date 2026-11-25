param(
    [Parameter(Mandatory)][string]$AppBinary,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string]$AppRevision,
    [Parameter(Mandatory)][string]$ManagementCredentialPath,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [switch]$IncludeNotepad,
    [switch]$ExpandedMatrix,
    [switch]$RealModelNotepad,
    [switch]$RealApps,
    [switch]$MockOnRealProfile,
    [ValidateRange(1, 80)][int]$RealRepeats = 1,
    [ValidateSet('original', 'short')][string]$RealFixture = 'original',
    [string]$ManifestDirectory,
    [string]$CancellationPath
)

$ErrorActionPreference = 'Stop'
if ($RealApps -and -not $RealModelNotepad) { throw 'RealApps requires RealModelNotepad.' }
if ($IncludeNotepad -and $RealModelNotepad) { throw 'Choose one Notepad mode.' }
if ($ExpandedMatrix -and ($IncludeNotepad -or $RealModelNotepad)) { throw 'Choose one fixture suite.' }
if ($MockOnRealProfile -and $RealModelNotepad) { throw 'MockOnRealProfile is a mock-only mode.' }
if (($RealRepeats -ne 1 -or $RealFixture -ne 'original') -and -not $RealModelNotepad) {
    throw 'RealRepeats and RealFixture require RealModelNotepad.'
}
$expectedUser = if ($RealModelNotepad -or $MockOnRealProfile) { 'ASREvalReal' } else { 'ASREval' }
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
    $session = New-PSSession -VMName $vm.Name -Credential $credential
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
        'windows_acceptance_target.html', 'windows_acceptance_terminal.py'
    foreach ($file in $files) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $file) -Destination "$guestRoot\scripts" -ToSession $session
    }
    foreach ($file in Get-ChildItem -LiteralPath (Join-Path $repo 'asr_worker') -File -Filter '*.py') {
        Copy-Item -LiteralPath $file.FullName -Destination "$guestRoot\asr_worker" -ToSession $session
    }
    if ($RealModelNotepad) {
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
    Assert-NotCancelled
    $externalRepeats = 2
    $externalTakes = if ($RealApps) { 2 * $externalRepeats } else { 0 }
    $limit = if ($RealModelNotepad) { 70 + 160 * ($RealRepeats + $externalTakes) } elseif ($ExpandedMatrix) { 925 } elseif ($IncludeNotepad) { 380 } else { 290 }
    $previousRun = Invoke-Command -Session $session -ArgumentList $guestRoot,$taskName,$digest,$AppRevision,$runId,($IncludeNotepad.IsPresent),($RealModelNotepad.IsPresent),$expectedUser,$RealRepeats,$limit,($MockOnRealProfile.IsPresent),($ExpandedMatrix.IsPresent),($RealApps.IsPresent) -ScriptBlock {
        param($root, $task, $expectedDigest, $revision, $owner, $notepad, $real, $userName, $repeats, $limit, $sharedMock, $expanded, $realApps)
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
        $externalCase = if ($realApps) { "--real-apps --real-repeats $repeats" } elseif ($real) { "--real-notepad --real-repeats $repeats" } elseif ($expanded) {
            '--expanded-matrix'
        } elseif ($notepad) { '--include-notepad' } else { '' }
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
    do {
        Assert-NotCancelled
        Start-Sleep -Milliseconds 750
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
    if ($RealApps) { $expectedCases += ',real-vscode,real-chrome' }
    if ($caseNames -ne $expectedCases -or @($guestResult.cases | Where-Object status -NotIn @('PASS','FAIL','BLOCKED')).Count) {
        throw 'Guest case coverage could not be verified.'
    }
    if ($RealModelNotepad) {
        foreach ($realCase in $guestResult.cases | Where-Object status -EQ 'PASS') {
            $count = if ($realCase.scenario -eq 'real-notepad') { $RealRepeats } else { $externalRepeats }
            if ($count -gt 1 -and (
                $realCase.requested_trials -ne $count -or @($realCase.trials).Count -ne $count -or
                ($realCase.trials.index -join ',') -ne ((1..$count) -join ',') -or
                @($realCase.trials | Where-Object status -NE 'PASS').Count)) {
                throw 'Guest real trial coverage could not be verified.'
            }
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
    $caseStatuses = @($guestResult.cases.status)
    $expectedStatus = if ($caseStatuses -contains 'FAIL') { 'FAIL' } elseif ($caseStatuses -contains 'BLOCKED') { 'BLOCKED' } else { 'PASS' }
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
                    $findRunProcesses = {
                        Get-CimInstance Win32_Process -Filter "Name='local-voice-input.exe' OR Name='python.exe' OR Name='uv.exe'" |
                            Where-Object {
                                $_.ExecutablePath -eq $appPath -or (
                                    $_.CommandLine -and (
                                        $_.CommandLine.IndexOf($entry, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
                                        $_.CommandLine.IndexOf($terminalEntry, [StringComparison]::OrdinalIgnoreCase) -ge 0))
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
                            if ($folder.Name -notmatch '^asr-(edge|chrome|code)-[0-9a-f]{32}$') { continue }
                            $ownerPath = Join-Path $folder.FullName 'window-owner.json'
                            if (-not (Test-Path -LiteralPath $ownerPath -PathType Leaf)) { continue }
                            $window = Get-Content -Raw -LiteralPath $ownerPath | ConvertFrom-Json
                            $processName = @{ edge = 'msedge'; chrome = 'chrome'; code = 'Code' }[$folder.Name.Split('-')[1]]
                            if ($window.name -ne $processName -or
                                $window.pid -notmatch '^\d+$' -or $window.start_ticks -notmatch '^\d+$') {
                                throw 'invalid external fixture ownership evidence'
                            }
                            $candidate = Get-Process -Id $window.pid -ErrorAction SilentlyContinue
                            if ($candidate -and $candidate.ProcessName -eq $window.name -and
                                $candidate.StartTime.ToUniversalTime() -ge $folder.CreationTimeUtc -and
                                "$($candidate.StartTime.ToUniversalTime().Ticks)" -eq $window.start_ticks) {
                                $cim = Get-CimInstance Win32_Process -Filter "ProcessId=$($candidate.Id)"
                                if ($cim -and $cim.CommandLine -and
                                    $cim.CommandLine.IndexOf($folder.FullName, [StringComparison]::OrdinalIgnoreCase) -ge 0 -and
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
                    Unregister-ScheduledTask -TaskName $task -Confirm:$false
                }
            }
        } catch {
            $report.status = 'BLOCKED'
            $report.cleanup_error = $_.Exception.Message
        } finally { Remove-PSSession -Session $session }
    }
    try { $report | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'host.json') -Encoding UTF8 }
    finally {
        if ($locked) { $controllerMutex.ReleaseMutex() }
        $controllerMutex.Dispose()
    }
}

Write-Output "$($report.status) $output"
if ($report.status -eq 'PASS') { exit 0 }
if ($report.status -eq 'FAIL') { exit 1 }
exit 2
