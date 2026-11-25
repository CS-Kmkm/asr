# Scoped UI Automation bridge for enrolled controls and ASR-Eval-owned fixtures.
# No global hooks, clipboard access, arbitrary scripts, or security UI handling.
$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class AcceptanceWindows {
    private delegate bool WindowCallback(IntPtr hwnd, IntPtr value);
    [DllImport("user32.dll")] private static extern bool EnumWindows(WindowCallback callback, IntPtr value);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int size);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int command);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint current, uint other, bool attach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    public static string Title(IntPtr hwnd) {
        var text = new StringBuilder(1024);
        GetWindowText(hwnd, text, text.Capacity);
        return text.ToString();
    }
    public static IntPtr[] FindVisibleWindows(string marker) {
        var windows = new List<IntPtr>();
        EnumWindows((hwnd, _) => {
            if (IsWindowVisible(hwnd) && Title(hwnd).Contains(marker))
                windows.Add(hwnd);
            return true;
        }, IntPtr.Zero);
        return windows.ToArray();
    }
}
'@
$targets = @{}
$bridgeRoot = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path $env:ASR_EVAL_UIA_SOURCE -Parent }
$notepadOwnership = @{ Launched = $null; Window = $null }
$fixtureOwnership = @{ Launched = $null; Window = $null }
function Get-Text($element) {
    if ($element.Current.IsPassword) { throw 'password target' }
    $pattern = $null
    if ($element.TryGetCurrentPattern([Windows.Automation.TextPattern]::Pattern, [ref]$pattern)) {
        $text = $pattern.DocumentRange.GetText(65537)
    } else {
        $text = $element.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).Current.Value
    }
    if ($text.Length -gt 65536) { throw 'oversized target' }
    return [string]$text
}
function Test-Identity($target) {
    $process = Get-Process -Id $target.ProcessId
    if ($process.StartTime.ToUniversalTime().Ticks -ne $target.StartTicks) { throw 'process changed' }
    [uint32]$ownerId = 0
    [void][AcceptanceWindows]::GetWindowThreadProcessId($target.Hwnd, [ref]$ownerId)
    if ($ownerId -ne $target.ProcessId) { throw 'window changed' }
    if (($target.Element.GetRuntimeId() -join ',') -ne $target.RuntimeId) { throw 'element changed' }
    if ($target.Element.Current.IsPassword -or -not $target.Element.Current.IsEnabled) { throw 'target unavailable' }
    if ($target.FixtureToken -and -not [AcceptanceWindows]::Title($target.Hwnd).Contains($target.FixtureToken)) {
        throw 'owned document changed'
    }
    if ($target.FixtureKind -in @('edge', 'chrome') -and
        -not (Test-EligibleEditor $target.Element $process.ProcessName 'primary' $target.Hwnd)) {
        throw 'owned browser left its local fixture'
    }
}
function Test-Focused($target) {
    Test-Identity $target
    if ([AcceptanceWindows]::GetForegroundWindow() -ne $target.Hwnd) { return $false }
    $focused = [Windows.Automation.AutomationElement]::FocusedElement
    return [Windows.Automation.Automation]::Compare($focused, $target.Element)
}
function Set-EditorFocus($hwnd, $element, $owned) {
    [void][AcceptanceWindows]::ShowWindow($hwnd, 9)
    [void][AcceptanceWindows]::SetForegroundWindow($hwnd)
    if ($owned -and [AcceptanceWindows]::GetForegroundWindow() -ne $hwnd) {
        [uint32]$foregroundPid = 0
        $foregroundThread = [AcceptanceWindows]::GetWindowThreadProcessId(
            [AcceptanceWindows]::GetForegroundWindow(), [ref]$foregroundPid)
        $current = [AcceptanceWindows]::GetCurrentThreadId()
        if ($foregroundThread -and $foregroundThread -ne $current -and
            [AcceptanceWindows]::AttachThreadInput($current, $foregroundThread, $true)) {
            try { [void][AcceptanceWindows]::SetForegroundWindow($hwnd) }
            finally { [void][AcceptanceWindows]::AttachThreadInput($current, $foregroundThread, $false) }
        }
    }
    $element.SetFocus()
}
function Test-LocalFixtureUrl($location) {
    try {
        $uri = [Uri]::new($location, [UriKind]::Absolute)
        $fixture = [IO.Path]::GetFullPath((Join-Path $bridgeRoot 'windows_acceptance_target.html'))
        return $uri.IsFile -and [IO.Path]::GetFullPath($uri.LocalPath).Equals($fixture, [StringComparison]::OrdinalIgnoreCase)
    } catch { return $false }
}

function Test-EligibleEditor($element, $processName, $slot, $ownedBrowserHwnd) {
    $classes = @()
    $localDocument = $false
    $fixtureDocument = $false
    $cursor = $element
    for ($depth = 0; $depth -lt 32 -and $null -ne $cursor; $depth++) {
        $classes += $cursor.Current.ClassName
        if ($cursor.Current.ClassName -match '(?i)xterm|terminal' -or $cursor.Current.AutomationId -match '(?i)xterm|terminal') { return $false }
        if ($cursor.Current.ControlType -eq [Windows.Automation.ControlType]::Document -and $cursor.Current.Name -eq 'ASR local acceptance target') {
            $fixtureDocument = $true
            $location = $null
            try { $location = $cursor.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).Current.Value } catch { }
            if (-not $location) {
                try { $location = $cursor.GetCurrentPattern([Windows.Automation.LegacyIAccessiblePattern]::Pattern).Current.Value } catch { }
            }
            $localDocument = Test-LocalFixtureUrl $location
        }
        $cursor = [Windows.Automation.TreeWalker]::ControlViewWalker.GetParent($cursor)
    }
    if ($processName -in @('msedge', 'chrome', 'firefox')) {
        if ($ownedBrowserHwnd -and $fixtureDocument -and -not $localDocument) {
            $root = [Windows.Automation.AutomationElement]::FromHandle($ownedBrowserHwnd)
            $addressClass = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::ClassNameProperty, 'OmniboxViewViews')
            $bars = @($root.FindAll([Windows.Automation.TreeScope]::Descendants, $addressClass))
            if ($bars.Count -eq 1) {
                $location = $bars[0].GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).Current.Value
                $localDocument = Test-LocalFixtureUrl $location
            }
        }
        return $element.Current.Name -eq "ASR evaluation $slot" -and $localDocument
    }
    if ($processName -eq 'Code') {
        return ($element.Current.ClassName -match 'inputarea|^native-edit-context$') -and
            [bool]($classes -match 'monaco-editor')
    }
    return $processName -eq 'notepad' -and (
        $element.Current.ClassName -match '^RichEdit' -or
        ($element.Current.ClassName -eq 'Edit' -and $element.Current.AutomationId -eq '15'))
}
function Assert-EvaluationGuest {
    if ($env:COMPUTERNAME -ne 'ASR-EVAL' -or $env:USERNAME -notin @('ASREval', 'ASREvalReal') -or
        (Get-ItemProperty 'HKLM:\HARDWARE\DESCRIPTION\System\BIOS').SystemProductName -ne 'Virtual Machine') {
        throw 'not the provisioned evaluation guest'
    }
}
function Start-OwnedNotepad($path, $ownership) {
    $evaluation = [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'ASR-Evaluation')) + '\'
    Assert-EvaluationGuest
    $fixture = Get-Item -LiteralPath $path
    if (-not $fixture.FullName.StartsWith($evaluation, [StringComparison]::OrdinalIgnoreCase) -or
        $fixture.Name -notmatch '^asr-fixture-[0-9a-f]{32}\.txt$' -or $fixture.Length -ne 0) {
        throw 'invalid Notepad fixture'
    }
    $session = (Get-Process -Id $PID).SessionId
    if (Get-Process notepad -ErrorAction SilentlyContinue | Where-Object SessionId -EQ $session) {
        throw 'Notepad was already running in this session'
    }
    $token = $fixture.BaseName
    $ownership.Launched = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\notepad.exe') `
        -ArgumentList ('"' + $fixture.FullName + '"') -PassThru
    $deadline = (Get-Date).AddSeconds(12)
    do {
        Start-Sleep -Milliseconds 150
        $matching = @(Get-Process notepad -ErrorAction SilentlyContinue | Where-Object {
            $_.SessionId -eq $session -and $_.MainWindowHandle -ne 0 -and $_.MainWindowTitle.Contains($token)
        })
    } while (-not $matching.Count -and (Get-Date) -lt $deadline)
    if ($matching.Count -ne 1) { throw 'could not identify one owned Notepad window' }
    $process = $matching[0]
    $ownership.Window = @{ Process = $process; Hwnd = $process.MainWindowHandle;
        StartTicks = $process.StartTime.ToUniversalTime().Ticks; Token = $token }
}

function Find-TerminalFixture($token, $session) {
    foreach ($hwnd in [AcceptanceWindows]::FindVisibleWindows($token)) {
        [uint32]$ownerId = 0
        [void][AcceptanceWindows]::GetWindowThreadProcessId($hwnd, [ref]$ownerId)
        $process = Get-Process -Id $ownerId -ErrorAction SilentlyContinue
        if ($process -and $process.ProcessName -eq 'WindowsTerminal' -and $process.SessionId -eq $session) {
            @{ Process = $process; Hwnd = $hwnd }
        }
    }
}

function Start-OwnedFixture($kind, $ownership) {
    Assert-EvaluationGuest
    $run = Split-Path $bridgeRoot -Parent
    $evaluation = Join-Path $env:LOCALAPPDATA 'ASR-Evaluation'
    if ((Split-Path $run -Parent) -ne $evaluation -or (Split-Path $run -Leaf) -notmatch '^[0-9a-f]{32}$') {
        throw 'not a staged evaluation run'
    }
    $token = 'asr-' + $kind + '-' + [guid]::NewGuid().ToString('N')
    $private = Join-Path (Join-Path $run 'target-profiles') $token
    New-Item -ItemType Directory -Path $private -Force | Out-Null
    $session = (Get-Process -Id $PID).SessionId
    $reader = $null
    switch ($kind) {
        { $_ -in @('edge', 'chrome') } {
            $exe = @{
                edge = 'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe'
                chrome = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
            }[$kind]
            $name = [IO.Path]::GetFileNameWithoutExtension($exe)
            $page = [Uri]::new((Join-Path $bridgeRoot 'windows_acceptance_target.html')).AbsoluteUri
            $arguments = "--user-data-dir=`"$private`" --no-first-run --no-default-browser-check --disable-background-networking --force-renderer-accessibility --new-window `"$page`""
            $token = 'ASR local acceptance target'
        }
        'code' {
            $exe = 'C:\ASR-Evaluation\tools\VSCode\Code.exe'
            $name = 'Code'
            $reader = Join-Path $private "$token.txt"
            [IO.File]::WriteAllText($reader, '')
            $userData = Join-Path $private 'user-data'
            $settings = New-Item -ItemType Directory -Path (Join-Path $userData 'User') -Force
            [IO.File]::WriteAllText((Join-Path $settings.FullName 'settings.json'),
                '{"editor.accessibilitySupport":"on","workbench.startupEditor":"none","files.autoSave":"afterDelay","files.autoSaveDelay":150,"telemetry.telemetryLevel":"off","workbench.tips.enabled":false,"chat.disableAIFeatures":true}')
            $arguments = "--new-window --disable-extensions --user-data-dir `"$userData`" `"$reader`""
        }
        'terminal' {
            $exe = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\wt.exe'
            $name = 'WindowsTerminal'
            $reader = Join-Path $private 'receiver.utf16'
            $python = 'C:\ASR-Evaluation\runtime\python\python.exe'
            $entry = Join-Path $bridgeRoot 'windows_acceptance_terminal.py'
            $arguments = "-w new new-tab --title $token --suppressApplicationTitle `"$python`" `"$entry`" `"$reader`""
        }
        default { throw 'unsupported fixture type' }
    }
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw 'fixture application absent' }
    $existing = @((Get-Process -Name $name -ErrorAction SilentlyContinue | Where-Object SessionId -EQ $session).Id)
    if ($kind -eq 'terminal' -and @(Find-TerminalFixture $token $session).Count) {
        throw 'terminal fixture token already in use'
    }
    $ownership.KeepLauncher = $kind -eq 'terminal'
    if ($ownership.KeepLauncher) {
        $ownership.TerminalToken = $token
        $ownership.TerminalSession = $session
    }
    $ownership.Launched = Start-Process -FilePath $exe -ArgumentList $arguments -PassThru
    $deadline = (Get-Date).AddSeconds(19)
    do {
        Start-Sleep -Milliseconds 200
        $matching = @(if ($kind -eq 'terminal') {
            Find-TerminalFixture $token $session
        } else {
            Get-Process -Name $name -ErrorAction SilentlyContinue | Where-Object {
                $_.SessionId -eq $session -and $_.Id -notin $existing -and $_.Path -eq $exe -and
                ($kind -notin @('edge', 'chrome') -or $_.Id -eq $ownership.Launched.Id) -and
                $_.MainWindowHandle -ne 0 -and $_.MainWindowTitle.Contains($token)
            } | ForEach-Object { @{ Process = $_; Hwnd = $_.MainWindowHandle } }
        })
    } while (($matching.Count -ne 1 -or ($reader -and -not (Test-Path -LiteralPath $reader))) -and (Get-Date) -lt $deadline)
    if ($matching.Count -ne 1) { throw 'could not identify the owned fixture window' }
    $process = $matching[0].Process
    $hwnd = $matching[0].Hwnd
    $startTicks = $process.StartTime.ToUniversalTime().Ticks
    $shared = $kind -eq 'terminal'
    $ownership.Window = @{ Process = $process; Hwnd = $hwnd; StartTicks = $startTicks; Token = $token; Shared = $shared }
    if (-not $shared) {
        $record = @{ pid = $process.Id; start_ticks = "$startTicks"; name = $name }
        [IO.File]::WriteAllText((Join-Path $private 'window-owner.json'), ($record | ConvertTo-Json -Compress))
    }
    if ($reader -and -not (Test-Path -LiteralPath $reader)) { throw 'fixture receiver not ready' }
    return @{ Process = $process; Hwnd = $hwnd; Kind = $kind; Reader = $reader; Token = $token }
}

function Get-TargetText($target) {
    if ($target.FixtureKind -in @('edge', 'chrome')) {
        $pattern = $null
        if ($target.Element.TryGetCurrentPattern([Windows.Automation.ValuePattern]::Pattern, [ref]$pattern)) {
            return [string]$pattern.Current.Value
        }
    }
    if (-not $target.Reader) { return Get-Text $target.Element }
    $file = Get-Item -LiteralPath $target.Reader
    if ($file.Length -gt 131072) { throw 'oversized fixture result' }
    $encoding = if ($target.FixtureKind -eq 'terminal') { [Text.Encoding]::Unicode } else { [Text.Encoding]::UTF8 }
    $stream = [IO.File]::Open($file.FullName, [IO.FileMode]::Open, [IO.FileAccess]::Read,
        ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    $reader = [IO.StreamReader]::new($stream, $encoding)
    try { return $reader.ReadToEnd() } finally { $reader.Dispose() }
}

function Close-OwnedWindow($ownership) {
    $owned = $ownership.Window
    if (-not $owned -and $ownership.TerminalToken) {
        $windows = @(Find-TerminalFixture $ownership.TerminalToken $ownership.TerminalSession)
        if ($windows.Count -gt 1) { throw 'terminal ownership is ambiguous' }
        if ($windows.Count -eq 1) {
            $late = $windows[0]
            $owned = @{ Process = $late.Process; Hwnd = $late.Hwnd; Token = $ownership.TerminalToken;
                StartTicks = $late.Process.StartTime.ToUniversalTime().Ticks; Shared = $true }
        }
    }
    if ($owned) {
        $process = Get-Process -Id $owned.Process.Id -ErrorAction SilentlyContinue
        if ($process -and [AcceptanceWindows]::IsWindow($owned.Hwnd)) {
            [uint32]$ownerId = 0
            [void][AcceptanceWindows]::GetWindowThreadProcessId($owned.Hwnd, [ref]$ownerId)
            if ($process.StartTime.ToUniversalTime().Ticks -ne $owned.StartTicks -or
                $ownerId -ne $process.Id -or -not [AcceptanceWindows]::Title($owned.Hwnd).Contains($owned.Token)) {
                throw 'owned window changed before cleanup'
            }
            if ($owned.Shared) {
                [void][AcceptanceWindows]::PostMessage($owned.Hwnd, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
                $deadline = (Get-Date).AddSeconds(2)
                while ([AcceptanceWindows]::IsWindow($owned.Hwnd) -and (Get-Date) -lt $deadline) {
                    Start-Sleep -Milliseconds 100
                }
                if ([AcceptanceWindows]::IsWindow($owned.Hwnd)) { throw 'owned terminal window did not close' }
            } else {
                Stop-Process -InputObject $process
                if (-not $process.WaitForExit(2000)) { throw 'owned window did not exit' }
            }
        }
        $ownership.Window = $null
    }
    if (-not $ownership.KeepLauncher -and $ownership.Launched -and -not $ownership.Launched.HasExited) {
        $ownership.Launched.Kill()
        if (-not $ownership.Launched.WaitForExit(2000)) { throw 'owned window launcher did not exit' }
    }
    $ownership.Launched = $null
    $ownership.TerminalToken = $null
}

try {
while ($null -ne ($line = [Console]::ReadLine())) {
    $stage = 'unsupported_or_changed_target'
    try {
        if ($line.Length -gt 8192) { throw 'oversized command' }
        $request = $line | ConvertFrom-Json
        if ($request.action -in @('capture', 'open_notepad', 'open_fixture')) {
            if ($request.target -notin @('primary', 'secondary') -or $targets.ContainsKey($request.target)) { throw 'invalid target' }
            $fixtureToken = $null
            $fixtureKind = $null
            $readerPath = $null
            if ($request.action -eq 'open_notepad') {
                $stage = 'owned_notepad_launch_failed'
                if ($request.target -ne 'primary' -or $notepadOwnership.Launched) { throw 'invalid Notepad slot' }
                Start-OwnedNotepad $request.path $notepadOwnership
                $ownedNotepad = $notepadOwnership.Window
                $stage = 'owned_notepad_editor_unavailable'
                $fixtureToken = $ownedNotepad.Token
                $hwnd = $ownedNotepad.Process.MainWindowHandle
                $root = [Windows.Automation.AutomationElement]::FromHandle($hwnd)
                $textCondition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::IsTextPatternAvailableProperty, $true)
                $editors = @($root.FindAll([Windows.Automation.TreeScope]::Descendants, $textCondition) | Where-Object {
                    $_.Current.ProcessId -eq $ownedNotepad.Process.Id -and
                    $_.Current.ClassName -match '^RichEdit' -and -not $_.Current.IsPassword
                })
                if ($editors.Count -ne 1) { throw 'owned Notepad editor is not unique' }
                $element = $editors[0]
                $stage = 'owned_notepad_focus_unavailable'
                Set-EditorFocus $hwnd $element $true
                $stage = 'owned_notepad_registration_failed'
            } elseif ($request.action -eq 'open_fixture') {
                $stage = 'owned_fixture_launch_failed'
                if ($request.target -ne 'primary' -or $fixtureOwnership.Launched) { throw 'invalid fixture slot' }
                $owned = Start-OwnedFixture $request.kind $fixtureOwnership
                $fixtureToken = $owned.Token
                $fixtureKind = $owned.Kind
                $readerPath = $owned.Reader
                $hwnd = $owned.Hwnd
                $stage = 'owned_fixture_editor_unavailable'
                $root = [Windows.Automation.AutomationElement]::FromHandle($hwnd)
                $property = [Windows.Automation.AutomationElement]::ClassNameProperty
                $value = switch ($fixtureKind) {
                    { $_ -in @('edge', 'chrome') } {
                        $property = [Windows.Automation.AutomationElement]::NameProperty
                        'ASR evaluation primary'
                    }
                    'code' {
                        $property = [Windows.Automation.AutomationElement]::NameProperty
                        [IO.Path]::GetFileName($readerPath)
                    }
                    'terminal' { 'TermControl' }
                }
                $condition = [Windows.Automation.PropertyCondition]::new($property, $value)
                $until = (Get-Date).AddSeconds(4)
                do {
                    $editors = @($root.FindAll([Windows.Automation.TreeScope]::Descendants, $condition) |
                        Where-Object {
                            $_.Current.IsKeyboardFocusable -and -not $_.Current.IsPassword -and
                            ($fixtureKind -ne 'code' -or $_.Current.ClassName -match 'inputarea|^native-edit-context$')
                        })
                    if ($editors.Count -eq 1) { break }
                    Start-Sleep -Milliseconds 150
                } while ((Get-Date) -lt $until)
                if ($editors.Count -ne 1) { throw 'owned fixture editor is not unique' }
                $element = $editors[0]
                $stage = 'owned_fixture_focus_unavailable'
                Set-EditorFocus $hwnd $element $true
            } else {
                $element = [Windows.Automation.AutomationElement]::FocusedElement
                $hwnd = [AcceptanceWindows]::GetForegroundWindow()
            }
            if ($fixtureKind) { $stage = 'owned_fixture_target_unproven' }
            $kind = $element.Current.ControlType
            if ($fixtureKind -ne 'terminal' -and $kind -ne [Windows.Automation.ControlType]::Edit -and
                $kind -ne [Windows.Automation.ControlType]::Document) { throw 'unsupported control' }
            [uint32]$ownerId = 0
            [void][AcceptanceWindows]::GetWindowThreadProcessId($hwnd, [ref]$ownerId)
            $process = Get-Process -Id $ownerId
            # Only dedicated scratch editors/browser pages, never shell prompts,
            # credential dialogs, app permission dialogs, or arbitrary controls.
            if ($fixtureKind -eq 'terminal') {
                if ($process.ProcessName -ne 'WindowsTerminal' -or $element.Current.ClassName -ne 'TermControl') {
                    throw 'unapproved terminal fixture element'
                }
            } else {
                if ($process.ProcessName -notin @('notepad', 'Code', 'msedge', 'chrome', 'firefox')) { throw 'unsupported application' }
                $ownedBrowser = if ($fixtureKind -in @('edge', 'chrome')) { $hwnd } else { $null }
                if (-not (Test-EligibleEditor $element $process.ProcessName $request.target $ownedBrowser)) { throw 'unapproved editor element' }
            }
            if ($fixtureKind) { $stage = 'owned_fixture_receiver_not_empty' }
            $emptyText = Get-TargetText @{ Element = $element; Reader = $readerPath; FixtureKind = $fixtureKind }
            if (-not $element.Current.IsKeyboardFocusable -or $emptyText -notin @('', "`r", "`n", "`r`n")) { throw 'empty scratch control required' }
            $runtimeId = $element.GetRuntimeId() -join ','
            foreach ($registered in $targets.Values) {
                if ($registered.RuntimeId -eq $runtimeId) { throw 'duplicate target' }
            }
            $target = @{ Element = $element; Hwnd = $hwnd; ProcessId = $ownerId
                StartTicks = $process.StartTime.ToUniversalTime().Ticks; RuntimeId = $runtimeId; EmptySuffix = $emptyText
                FixtureToken = $fixtureToken; FixtureKind = $fixtureKind; Reader = $readerPath }
            if ($fixtureKind) { $stage = 'owned_fixture_focus_unavailable' }
            if (-not (Test-Focused $target)) { throw 'focus changed during enrollment' }
            $targets[$request.target] = $target
            $version = $null
            try {
                $candidateVersion = $process.MainModule.FileVersionInfo.FileVersion
                if ($candidateVersion -match '^\d[\d. -]*$') { $version = $candidateVersion }
            } catch { }
            $result = @{ ok = $true; process = $process.ProcessName; version = $version; window = $hwnd.ToInt64() }
        } elseif ($request.action -eq 'close_notepad' -and $notepadOwnership.Launched) {
            $stage = 'owned_notepad_cleanup_failed'
            Close-OwnedWindow $notepadOwnership
            $result = @{ ok = $true }
        } elseif ($request.action -eq 'close_fixture' -and $fixtureOwnership.Launched) {
            $stage = 'owned_fixture_cleanup_failed'
            Close-OwnedWindow $fixtureOwnership
            $result = @{ ok = $true }
        } else {
            if (-not $targets.ContainsKey($request.target)) { throw 'unregistered target' }
            $target = $targets[$request.target]
            Test-Identity $target
            switch ($request.action) {
                'focus' {
                    Set-EditorFocus $target.Hwnd $target.Element ([bool]$target.FixtureToken)
                    $result = @{ ok = $true; focused = (Test-Focused $target) }
                }
                'focused' { $result = @{ ok = $true; focused = (Test-Focused $target) } }
                'read' {
                    $text = Get-TargetText $target
                    # Remove at most the provider's enrolled empty-range suffix,
                    # never arbitrary user/inserted trailing line breaks.
                    if ($target.EmptySuffix.Length -and $text.EndsWith($target.EmptySuffix, [StringComparison]::Ordinal)) {
                        $text = $text.Substring(0, $text.Length - $target.EmptySuffix.Length)
                    }
                    $result = @{ ok = $true; text = $text }
                }
                'select_all' {
                    if (-not (Test-Focused $target)) { throw 'focus mismatch' }
                    $pattern = $target.Element.GetCurrentPattern([Windows.Automation.TextPattern]::Pattern)
                    $pattern.DocumentRange.Select()
                    if (-not (Test-Focused $target)) { throw 'focus changed' }
                    $result = @{ ok = $true }
                }
                'minimize' {
                    [void][AcceptanceWindows]::ShowWindow($target.Hwnd, 7)
                    $result = @{ ok = $true }
                }
                default { throw 'unknown action' }
            }
        }
        [Console]::WriteLine(($result | ConvertTo-Json -Compress))
    } catch {
        # Do not serialize provider exception messages (may contain private text).
        [Console]::WriteLine((@{ ok = $false; error = $stage } | ConvertTo-Json -Compress))
    }
}
} finally {
    try { Close-OwnedWindow $fixtureOwnership }
    finally { Close-OwnedWindow $notepadOwnership }
}
