//! Platform-independent decisions about which process a captured foreground
//! window belongs to and whether it is a protected surface. The Windows
//! backend gathers window classes and ids; these functions decide.

/// Frame window that ApplicationFrameHost draws around UWP apps.
pub(super) const FRAME_HOST_CLASS: &str = "ApplicationFrameWindow";
/// Window class of a UWP app's own content window.
pub(super) const CORE_WINDOW_CLASS: &str = "Windows.UI.Core.CoreWindow";

/// A child window of the foreground window, by class and owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ChildWindow {
    pub(super) class_name: String,
    pub(super) process_id: u32,
    pub(super) thread_id: u32,
}

pub(super) fn is_frame_host(class_name: Option<&str>) -> bool {
    class_name.is_some_and(|class_name| class_name.eq_ignore_ascii_case(FRAME_HOST_CLASS))
}

/// UWP apps (Settings, Calculator, To Do) run inside an ApplicationFrameHost
/// frame. The foreground window and its thread belong to the frame host, but
/// the focused field belongs to the app process that owns the hosted
/// CoreWindow. Returns that process and thread, or `None` while no app is
/// hosted (for example during launch or after the app is suspended).
pub(super) fn hosted_app(frame_process: u32, children: &[ChildWindow]) -> Option<(u32, u32)> {
    children
        .iter()
        .find(|child| {
            child.class_name.eq_ignore_ascii_case(CORE_WINDOW_CLASS)
                && child.process_id != 0
                && child.thread_id != 0
                && child.process_id != frame_process
        })
        .map(|child| (child.process_id, child.thread_id))
}

/// Window classes that host Windows credential prompts. A CoreWindow inside
/// an app frame is an ordinary UWP app, whose password fields UI Automation
/// identifies like any other app's; only a standalone CoreWindow is treated
/// as a possible credential prompt.
pub(super) fn is_credential_surface(class_name: &str, hosted_app: bool) -> bool {
    class_name.eq_ignore_ascii_case("Credential Dialog Xaml Host")
        || class_name.eq_ignore_ascii_case("CredentialUIBroker")
        || !hosted_app && class_name.eq_ignore_ascii_case(CORE_WINDOW_CLASS)
}

/// A credential surface whose password state UI Automation cannot report is
/// treated as protected.
pub(super) fn is_secure(
    style_password: bool,
    credential_surface: bool,
    uia_password: Option<bool>,
) -> bool {
    style_password || uia_password == Some(true) || credential_surface && uia_password.is_none()
}

/// A bounded password query may fail because the shared worker is busy or
/// its deadline expired. Preserve that uncertainty: only an explicit false
/// property can permit a target without another protected-surface signal.
pub(super) fn secure_after_password_query(
    style_password: bool,
    credential_surface: bool,
    password_query: Result<bool, ()>,
) -> bool {
    password_query
        .map(|password| is_secure(style_password, credential_surface, Some(password)))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_password_query_failure_protects_rich_and_credential_controls() {
        for credential in [false, true] {
            assert!(secure_after_password_query(false, credential, Err(())));
            assert!(secure_after_password_query(false, credential, Ok(true)));
            assert!(!secure_after_password_query(false, credential, Ok(false)));
            assert!(secure_after_password_query(true, credential, Ok(false)));
        }
    }

    fn child(class_name: &str, process_id: u32, thread_id: u32) -> ChildWindow {
        ChildWindow {
            class_name: class_name.into(),
            process_id,
            thread_id,
        }
    }

    #[test]
    fn a_frame_host_resolves_to_the_hosted_app_process() {
        assert!(is_frame_host(Some("ApplicationFrameWindow")));
        assert!(!is_frame_host(Some("Notepad")));
        assert!(!is_frame_host(None));
        // The frame's own title bar child belongs to the frame host.
        let children = [
            child("ApplicationFrameTitleBarWindow", 10, 11),
            child(CORE_WINDOW_CLASS, 10, 12),
            child(CORE_WINDOW_CLASS, 20, 21),
        ];
        assert_eq!(hosted_app(10, &children), Some((20, 21)));
    }

    #[test]
    fn a_frame_without_a_hosted_app_keeps_the_frame_identity() {
        assert_eq!(hosted_app(10, &[]), None);
        assert_eq!(hosted_app(10, &[child(CORE_WINDOW_CLASS, 10, 12)]), None);
        assert_eq!(hosted_app(10, &[child(CORE_WINDOW_CLASS, 0, 12)]), None);
        assert_eq!(hosted_app(10, &[child("Other", 20, 21)]), None);
    }

    #[test]
    fn a_hosted_core_window_is_not_mistaken_for_a_credential_prompt() {
        assert!(!is_credential_surface(CORE_WINDOW_CLASS, true));
        assert!(is_credential_surface(CORE_WINDOW_CLASS, false));
        assert!(is_credential_surface("Credential Dialog Xaml Host", true));
        assert!(is_credential_surface("CredentialUIBroker", false));
        assert!(!is_credential_surface("Edit", false));
        // With the hosted app's process, UI Automation answers and decides.
        assert!(!is_secure(false, false, Some(false)));
        assert!(is_secure(false, false, Some(true)));
        assert!(is_secure(true, false, Some(false)));
        // An unanswered credential surface stays protected.
        assert!(is_secure(false, true, None));
        assert!(!is_secure(false, false, None));
    }
}
