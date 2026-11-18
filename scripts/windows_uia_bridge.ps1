# Scoped UI Automation bridge. Run by windows_acceptance.py; no global hooks,
# clipboard access, arbitrary scripts, process launch, or security UI handling.
$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class AcceptanceWindows {
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int command);
}
'@
$targets = @{}
function Get-Text($element) {
    if ($element.Current.IsPassword) { throw 'password target' }
    $pattern = $element.GetCurrentPattern([Windows.Automation.TextPattern]::Pattern)
    $text = $pattern.DocumentRange.GetText(65537)
    if ($text.Length -gt 65536) { throw 'oversized target' }
    return $text
}
function Test-Identity($target) {
    $process = Get-Process -Id $target.ProcessId
    if ($process.StartTime.ToUniversalTime().Ticks -ne $target.StartTicks) { throw 'process changed' }
    [uint32]$ownerId = 0
    [void][AcceptanceWindows]::GetWindowThreadProcessId($target.Hwnd, [ref]$ownerId)
    if ($ownerId -ne $target.ProcessId) { throw 'window changed' }
    if (($target.Element.GetRuntimeId() -join ',') -ne $target.RuntimeId) { throw 'element changed' }
    if ($target.Element.Current.IsPassword -or -not $target.Element.Current.IsEnabled) { throw 'target unavailable' }
}
function Test-Focused($target) {
    Test-Identity $target
    if ([AcceptanceWindows]::GetForegroundWindow() -ne $target.Hwnd) { return $false }
    $focused = [Windows.Automation.AutomationElement]::FocusedElement
    return [Windows.Automation.Automation]::Compare($focused, $target.Element)
}
function Test-EligibleEditor($element, $processName, $slot) {
    $classes = @()
    $localDocument = $false
    $cursor = $element
    for ($depth = 0; $depth -lt 32 -and $null -ne $cursor; $depth++) {
        $classes += $cursor.Current.ClassName
        if ($cursor.Current.ClassName -match '(?i)xterm|terminal' -or $cursor.Current.AutomationId -match '(?i)xterm|terminal') { return $false }
        if ($cursor.Current.ControlType -eq [Windows.Automation.ControlType]::Document -and $cursor.Current.Name -eq 'ASR local acceptance target') {
            $location = $null
            try { $location = $cursor.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).Current.Value } catch { }
            if (-not $location) {
                try { $location = $cursor.GetCurrentPattern([Windows.Automation.LegacyIAccessiblePattern]::Pattern).Current.Value } catch { }
            }
            try {
                $uri = [Uri]::new($location, [UriKind]::Absolute)
                $fixture = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'windows_acceptance_target.html'))
                if ($uri.IsFile -and [IO.Path]::GetFullPath($uri.LocalPath).Equals($fixture, [StringComparison]::OrdinalIgnoreCase)) { $localDocument = $true }
            } catch { }
        }
        $cursor = [Windows.Automation.TreeWalker]::ControlViewWalker.GetParent($cursor)
    }
    if ($processName -in @('msedge', 'chrome', 'firefox')) {
        return $element.Current.Name -eq "ASR evaluation $slot" -and $localDocument
    }
    if ($processName -eq 'Code') {
        return ($element.Current.ClassName -match 'inputarea') -and [bool]($classes -match 'monaco-editor')
    }
    return $processName -eq 'notepad' -and (
        $element.Current.ClassName -match '^RichEdit' -or
        ($element.Current.ClassName -eq 'Edit' -and $element.Current.AutomationId -eq '15'))
}
while ($null -ne ($line = [Console]::ReadLine())) {
    try {
        if ($line.Length -gt 8192) { throw 'oversized command' }
        $request = $line | ConvertFrom-Json
        if ($request.action -eq 'capture') {
            if ($request.target -notin @('primary', 'secondary') -or $targets.ContainsKey($request.target)) { throw 'invalid target' }
            $element = [Windows.Automation.AutomationElement]::FocusedElement
            $kind = $element.Current.ControlType
            if ($kind -ne [Windows.Automation.ControlType]::Edit -and $kind -ne [Windows.Automation.ControlType]::Document) { throw 'unsupported control' }
            $hwnd = [AcceptanceWindows]::GetForegroundWindow()
            [uint32]$ownerId = 0
            [void][AcceptanceWindows]::GetWindowThreadProcessId($hwnd, [ref]$ownerId)
            $process = Get-Process -Id $ownerId
            # Only dedicated scratch editors/browser pages, never shell prompts,
            # credential dialogs, app permission dialogs, or arbitrary controls.
            if ($process.ProcessName -notin @('notepad', 'Code', 'msedge', 'chrome', 'firefox')) { throw 'unsupported application' }
            if (-not (Test-EligibleEditor $element $process.ProcessName $request.target)) { throw 'unapproved editor element' }
            $emptyText = Get-Text $element
            if (-not $element.Current.IsKeyboardFocusable -or $emptyText -notin @('', "`r", "`n", "`r`n")) { throw 'empty scratch control required' }
            $runtimeId = $element.GetRuntimeId() -join ','
            foreach ($registered in $targets.Values) {
                if ($registered.RuntimeId -eq $runtimeId) { throw 'duplicate target' }
            }
            $target = @{ Element = $element; Hwnd = $hwnd; ProcessId = $ownerId
                StartTicks = $process.StartTime.ToUniversalTime().Ticks; RuntimeId = $runtimeId; EmptySuffix = $emptyText }
            if (-not (Test-Focused $target)) { throw 'focus changed during enrollment' }
            $targets[$request.target] = $target
            $version = $null
            try {
                $candidateVersion = $process.MainModule.FileVersionInfo.FileVersion
                if ($candidateVersion -match '^\d[\d. -]*$') { $version = $candidateVersion }
            } catch { }
            $result = @{ ok = $true; process = $process.ProcessName; version = $version; window = $hwnd.ToInt64() }
        } else {
            if (-not $targets.ContainsKey($request.target)) { throw 'unregistered target' }
            $target = $targets[$request.target]
            Test-Identity $target
            switch ($request.action) {
                'focus' {
                    [void][AcceptanceWindows]::ShowWindow($target.Hwnd, 9)
                    [void][AcceptanceWindows]::SetForegroundWindow($target.Hwnd)
                    $target.Element.SetFocus()
                    $result = @{ ok = $true; focused = (Test-Focused $target) }
                }
                'focused' { $result = @{ ok = $true; focused = (Test-Focused $target) } }
                'read' {
                    $text = Get-Text $target.Element
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
        [Console]::WriteLine('{"ok":false,"error":"unsupported_or_changed_target"}')
    }
}
