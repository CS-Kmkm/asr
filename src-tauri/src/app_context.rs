use crate::{injection::TargetWindow, types::AppContext};

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
