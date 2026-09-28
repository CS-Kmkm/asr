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

pub(crate) fn from_target(target: &TargetWindow) -> AppContext {
    #[cfg(target_os = "windows")]
    let stem = executable_path(target.process_id).and_then(|path| normalize_executable_stem(&path));
    #[cfg(not(target_os = "windows"))]
    let stem: Option<String> = None;
    AppContext {
        category: category_for_stem(stem.as_deref()),
        app_key: stem,
    }
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
    fn lookup_failure_falls_back_without_a_path_or_app_key() {
        let context = from_target(&TargetWindow {
            window_handle: 0,
            control_handle: 0,
            process_id: 0,
            thread_id: 0,
            is_secure: false,
        });
        assert_eq!(context.app_key, None);
        assert_eq!(context.category, "other");
    }
}
