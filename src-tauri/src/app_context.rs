use crate::{injection::TargetWindow, types::AppContext};

pub(crate) fn recording_target<'a>(
    mode: crate::state::PipelineMode,
    dictate: Option<&'a TargetWindow>,
    edit: Option<&'a TargetWindow>,
    ask: Option<&'a TargetWindow>,
) -> Option<&'a TargetWindow> {
    match mode {
        crate::state::PipelineMode::Dictate | crate::state::PipelineMode::Translate => dictate,
        crate::state::PipelineMode::Edit => edit,
        crate::state::PipelineMode::Ask => ask,
        crate::state::PipelineMode::SelectedText => None,
    }
}

/// Metric context does not implicitly opt Edit/Ask into scoped ASR hints.
pub(crate) fn asr_prompt_context(
    mode: crate::state::PipelineMode,
    context: Option<&AppContext>,
) -> Option<&AppContext> {
    matches!(
        mode,
        crate::state::PipelineMode::Dictate | crate::state::PipelineMode::Translate
    )
    .then_some(context)
    .flatten()
}

pub(crate) fn normalize_executable_stem(path: &str) -> Option<String> {
    let basename = path.rsplit(['\\', '/']).next()?;
    let lowered = basename.to_ascii_lowercase();
    let stem = lowered.strip_suffix(".exe").unwrap_or(&lowered).trim();
    (!stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then_some(stem.to_owned())
}

pub(crate) fn category_for_stem(stem: Option<&str>) -> String {
    let stem = stem.unwrap_or_default();
    let category = if ["chrome", "msedge", "firefox", "brave", "opera"].contains(&stem) {
        "browser"
    } else if ["outlook", "olk", "thunderbird", "mail"].contains(&stem) {
        "email"
    } else if ["slack", "teams", "ms-teams", "discord", "telegram"].contains(&stem) {
        "messaging"
    } else if ["code", "devenv", "idea64", "rider", "sublime_text"].contains(&stem) {
        "development"
    } else if ["winword", "excel", "powerpnt", "notepad"].contains(&stem) {
        "document"
    } else {
        "other"
    };
    category.into()
}

#[cfg(target_os = "windows")]
fn executable_path(process_id: u32) -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()? };
    let mut buffer = [0u16; 1024];
    let mut size = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            windows::Win32::System::Threading::PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    unsafe {
        let _ = CloseHandle(HANDLE(handle.0));
    }
    result
        .ok()
        .map(|_| String::from_utf16_lossy(&buffer[..size as usize]))
}

/// Returns `None` when the process lookup fails, so routing falls back to
/// global dictionary entries and the global profile only. An identified
/// executable without a known category is `category:other`.
pub(crate) fn from_executable_path(path: Option<&str>) -> Option<AppContext> {
    let stem = normalize_executable_stem(path?);
    Some(AppContext {
        category: category_for_stem(stem.as_deref()),
        app_key: stem,
    })
}

pub(crate) fn from_target(target: &TargetWindow) -> Option<AppContext> {
    // This app's own window is not a dictation context.
    if target.is_this_app() {
        return None;
    }
    #[cfg(target_os = "windows")]
    let path = executable_path(target.process_id);
    #[cfg(not(target_os = "windows"))]
    let path: Option<String> = {
        let _ = target;
        None
    };
    from_executable_path(path.as_deref())
}

/// The coarse category recorded in History; unknown when the lookup failed.
pub(crate) fn history_category(context: Option<&AppContext>) -> Option<&str> {
    context.map(|context| context.category.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_and_ask_context_uses_the_session_target_without_a_dictation_target() {
        use crate::state::PipelineMode;
        let edit = TargetWindow {
            window_handle: 1,
            control_handle: 1,
            process_id: 1,
            thread_id: 1,
            is_secure: false,
        };
        let ask = TargetWindow {
            window_handle: 2,
            ..edit.clone()
        };
        assert_eq!(
            recording_target(PipelineMode::Edit, None, Some(&edit), None)
                .map(|target| target.window_handle),
            Some(1)
        );
        assert_eq!(
            recording_target(PipelineMode::Ask, None, None, Some(&ask))
                .map(|target| target.window_handle),
            Some(2)
        );
        assert!(recording_target(PipelineMode::Ask, Some(&edit), Some(&edit), None).is_none());
        let context = from_executable_path(Some("outlook.exe")).unwrap();
        assert!(asr_prompt_context(PipelineMode::Edit, Some(&context)).is_none());
        assert!(asr_prompt_context(PipelineMode::Ask, Some(&context)).is_none());
        assert_eq!(
            asr_prompt_context(PipelineMode::Dictate, Some(&context))
                .unwrap()
                .category,
            "email"
        );
    }
    #[test]
    fn normalizes_stem_without_exposing_path() {
        assert_eq!(
            normalize_executable_stem(r"C:\Program Files\Code.exe").as_deref(),
            Some("code")
        );
        assert_eq!(
            normalize_executable_stem(r"C:\Tools\MyEditor.EXE").as_deref(),
            Some("myeditor")
        );
    }
    #[test]
    fn maps_known_categories_and_fallback() {
        assert_eq!(category_for_stem(Some("msedge")), "browser");
        assert_eq!(category_for_stem(Some("ms-teams")), "messaging");
        assert_eq!(category_for_stem(Some("olk")), "email");
        assert_eq!(category_for_stem(Some("unknown")), "other");
        assert_eq!(category_for_stem(None), "other");
    }

    #[test]
    fn lookup_failure_yields_no_context_or_history_category() {
        let context = from_target(&TargetWindow {
            window_handle: 0,
            control_handle: 0,
            process_id: 0,
            thread_id: 0,
            is_secure: false,
        });
        assert_eq!(context, None);
        assert_eq!(from_target(&TargetWindow::this_app()), None);
        assert_eq!(from_executable_path(None), None);
        assert_eq!(history_category(context.as_ref()), None);
    }

    #[test]
    fn identified_unclassified_app_stays_category_other() {
        let context = from_executable_path(Some(r"C:\Tools\MyEditor.exe")).unwrap();
        assert_eq!(context.app_key.as_deref(), Some("myeditor"));
        assert_eq!(context.category, "other");
        assert_eq!(history_category(Some(&context)), Some("other"));

        // A path whose stem cannot be keyed is still an identified app.
        let context = from_executable_path(Some(r"C:\Tools\メモ.exe")).unwrap();
        assert_eq!(context.app_key, None);
        assert_eq!(history_category(Some(&context)), Some("other"));

        let context = from_executable_path(Some(r"C:\Program Files\Code.exe")).unwrap();
        assert_eq!(history_category(Some(&context)), Some("development"));
    }
}
