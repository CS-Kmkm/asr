use super::{normalize_text, InjectionError, TargetText, TargetWindow};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationTextRange, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
    TextUnit_Character, UIA_TextPatternId,
};

const MAX_TEXT_UNITS: i32 = 1_048_576;

struct Apartment(bool);
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

struct TextControl {
    // COM references must be released before apartment teardown.
    element: IUIAutomationElement,
    pattern: IUIAutomationTextPattern,
    _automation: IUIAutomation,
    _apartment: Apartment,
}

fn unavailable() -> InjectionError {
    InjectionError::BackendFailure("the focused control does not expose a verifiable text range")
}

impl TextControl {
    fn focused(target: &TargetWindow) -> Result<Self, InjectionError> {
        let apartment =
            Apartment(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok());
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| unavailable())?;
        let element = unsafe { automation.GetFocusedElement() }.map_err(|_| unavailable())?;
        if unsafe { element.CurrentProcessId() }.map_err(|_| unavailable())?
            != target.process_id as i32
        {
            return Err(InjectionError::TargetChanged);
        }
        if unsafe { element.CurrentIsPassword() }
            .map_err(|_| unavailable())?
            .as_bool()
        {
            return Err(InjectionError::SecureTarget);
        }
        let pattern =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }
                .map_err(|_| unavailable())?;
        Ok(Self {
            element,
            pattern,
            _automation: automation,
            _apartment: apartment,
        })
    }

    fn selection(&self) -> Result<IUIAutomationTextRange, InjectionError> {
        let selections = unsafe { self.pattern.GetSelection() }.map_err(|_| unavailable())?;
        if unsafe { selections.Length() }.map_err(|_| unavailable())? != 1 {
            return Err(unavailable());
        }
        unsafe { selections.GetElement(0) }.map_err(|_| unavailable())
    }

    fn state(&self) -> Result<TargetText, InjectionError> {
        let selection = self.selection()?;
        let document = unsafe { self.pattern.DocumentRange() }.map_err(|_| unavailable())?;
        let before = unsafe { document.Clone() }.map_err(|_| unavailable())?;
        let after = unsafe { document.Clone() }.map_err(|_| unavailable())?;
        unsafe {
            before
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_End,
                    &selection,
                    TextPatternRangeEndpoint_Start,
                )
                .map_err(|_| unavailable())?;
            after
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_Start,
                    &selection,
                    TextPatternRangeEndpoint_End,
                )
                .map_err(|_| unavailable())?;
        }
        Ok(TargetText {
            identity: runtime_id(&self.element)?,
            before: read_range(&before)?,
            selected: read_range(&selection)?,
            after: read_range(&after)?,
        })
    }
}

fn runtime_id(element: &IUIAutomationElement) -> Result<Vec<i32>, InjectionError> {
    struct Array(*mut SAFEARRAY);
    impl Drop for Array {
        fn drop(&mut self) {
            unsafe {
                let _ = SafeArrayDestroy(self.0);
            }
        }
    }
    let array = Array(unsafe { element.GetRuntimeId() }.map_err(|_| unavailable())?);
    if array.0.is_null() {
        return Err(unavailable());
    }
    let lower = unsafe { SafeArrayGetLBound(array.0, 1) }.map_err(|_| unavailable())?;
    let upper = unsafe { SafeArrayGetUBound(array.0, 1) }.map_err(|_| unavailable())?;
    if upper < lower || i64::from(upper) - i64::from(lower) > 64 {
        return Err(unavailable());
    }
    let mut result = Vec::new();
    for index in lower..=upper {
        let mut value = 0i32;
        unsafe { SafeArrayGetElement(array.0, &index, (&mut value as *mut i32).cast()) }
            .map_err(|_| unavailable())?;
        result.push(value);
    }
    Ok(result)
}

fn read_range(range: &IUIAutomationTextRange) -> Result<String, InjectionError> {
    Ok(normalize_text(&read_range_raw(range)?))
}

fn read_range_raw(range: &IUIAutomationTextRange) -> Result<String, InjectionError> {
    let text = unsafe { range.GetText(MAX_TEXT_UNITS + 1) }.map_err(|_| unavailable())?;
    if text.len() > MAX_TEXT_UNITS as usize {
        return Err(unavailable());
    }
    Ok(text.to_string())
}

pub(super) fn read(target: &TargetWindow) -> Result<TargetText, InjectionError> {
    TextControl::focused(target)?.state()
}

pub(super) fn select_recent(
    target: &TargetWindow,
    expected: &TargetText,
    text: &str,
) -> Result<bool, InjectionError> {
    let control = TextControl::focused(target)?;
    if !control.state()?.same_content(expected) {
        return Ok(false);
    }
    let Some(selected) = expected.select_recent(text) else {
        return Ok(false);
    };
    let range = control.selection()?;
    let text = normalize_text(text);
    // UIA providers disagree on grapheme/caret units, and some spend two units
    // on CRLF even though read_range normalizes it to LF. Walk until the
    // provider reaches the document boundary, accumulating only each newly
    // added unit so cross-process text transfer remains linear.
    let mut raw_actual = String::new();
    loop {
        let previous = unsafe { range.Clone() }.map_err(|_| unavailable())?;
        if unsafe {
            range.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -1)
        }
        .map_err(|_| unavailable())?
            == 0
        {
            return Ok(false);
        }
        let added = unsafe { range.Clone() }.map_err(|_| unavailable())?;
        unsafe {
            added
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_End,
                    &previous,
                    TextPatternRangeEndpoint_Start,
                )
                .map_err(|_| unavailable())?;
        }
        raw_actual.insert_str(0, &read_range_raw(&added)?);
        let actual = normalize_text(&raw_actual);
        if actual == text {
            // Verify the complete candidate range immediately before Select().
            if read_range(&range)? != text || !control.state()?.same_content(expected) {
                return Ok(false);
            }
            unsafe { range.Select() }.map_err(|_| unavailable())?;
            return Ok(control.state()?.same_content(&selected));
        }
        if !text.ends_with(&actual) {
            return Ok(false);
        }
    }
}
