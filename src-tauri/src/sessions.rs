//! Selection snapshots and input monitors scoped to an Edit or Ask operation.
use crate::{
    ask::AskContextKind,
    injection::{
        self, InjectionOptions, SelectedText, SystemTextInjector, TargetWindow, TextInjector,
    },
    input_monitor::InputMonitor,
    types::Settings,
};
use std::sync::Arc;

pub(crate) struct EditSession {
    pub(crate) selection: SelectedText,
    pub(crate) injector: SystemTextInjector,
    pub(crate) monitor: Arc<InputMonitor>,
    pub(crate) checkpoint: u64,
}

impl EditSession {
    pub(crate) fn new(
        settings: &Settings,
        active_hotkey: &str,
        from_shortcut: bool,
    ) -> Result<Self, String> {
        let injector = SystemTextInjector::new(InjectionOptions {
            restore_clipboard: settings.clipboard_restore,
        });
        Self::with_injector(
            settings,
            active_hotkey,
            from_shortcut,
            injector,
            Arc::new(InputMonitor::default()),
        )
    }

    fn with_injector(
        settings: &Settings,
        active_hotkey: &str,
        from_shortcut: bool,
        injector: SystemTextInjector,
        monitor: Arc<InputMonitor>,
    ) -> Result<Self, String> {
        let selection = injector
            .capture_selection()
            .map_err(|_| "Select text in a supported foreground edit control.".to_string())?;
        if !monitor.start_for_recording(active_hotkey, &settings.shortcuts.edit, from_shortcut) {
            return Err("Speak to edit could not monitor the original selection safely.".into());
        }
        let checkpoint = monitor
            .checkpoint()
            .ok_or("Speak to edit could not monitor the original selection safely.")?;
        Ok(Self {
            selection,
            injector,
            monitor,
            checkpoint,
        })
    }
}

impl Drop for EditSession {
    fn drop(&mut self) {
        self.monitor.shutdown();
    }
}

pub(crate) enum AskCapture {
    Selected(SelectedText),
    Caret(TargetWindow),
    Unavailable,
}

// The answer panel and all other app windows share our process. Never use
// their focused text as a fresh Ask source or insertion target.
pub(crate) fn is_external_ask_target(target: &TargetWindow) -> bool {
    target.process_id != std::process::id()
}

pub(crate) struct AskSession {
    pub(crate) capture: AskCapture,
    pub(crate) injector: SystemTextInjector,
    pub(crate) monitor: Option<Arc<InputMonitor>>,
    pub(crate) checkpoint: Option<u64>,
}

impl AskSession {
    pub(crate) fn new(settings: &Settings, active_hotkey: &str, from_shortcut: bool) -> Self {
        let injector = SystemTextInjector::new(InjectionOptions {
            restore_clipboard: settings.clipboard_restore,
        });
        Self::with_injector(
            settings,
            active_hotkey,
            from_shortcut,
            injector,
            Arc::new(InputMonitor::default()),
        )
    }

    fn with_injector(
        settings: &Settings,
        active_hotkey: &str,
        from_shortcut: bool,
        injector: SystemTextInjector,
        monitor: Arc<InputMonitor>,
    ) -> Self {
        // Only the known "no selection" result may become a caret capture.
        // Any inaccessible/changed selection becomes panel-only, never an
        // insertion target.
        let capture = match injector.capture_selection() {
            Ok(selection) if is_external_ask_target(selection.target()) => {
                AskCapture::Selected(selection)
            }
            Ok(_) => AskCapture::Unavailable,
            Err(injection::InjectionError::NoSelection) => match injector.capture_target() {
                Ok(target) if is_external_ask_target(&target) => AskCapture::Caret(target),
                Ok(_) => AskCapture::Unavailable,
                Err(_) => AskCapture::Unavailable,
            },
            Err(_) => AskCapture::Unavailable,
        };
        let monitor = match &capture {
            AskCapture::Selected(_) | AskCapture::Caret(_) => {
                if monitor.start_for_recording(
                    active_hotkey,
                    &settings.shortcuts.ask,
                    from_shortcut,
                ) {
                    Some(monitor)
                } else {
                    None
                }
            }
            AskCapture::Unavailable => None,
        };
        let checkpoint = monitor.as_ref().and_then(|monitor| monitor.checkpoint());
        let capture = if matches!(&capture, AskCapture::Caret(_)) && checkpoint.is_none() {
            AskCapture::Unavailable
        } else {
            capture
        };
        Self {
            capture,
            injector,
            monitor,
            checkpoint,
        }
    }

    pub(crate) fn kind(&self) -> AskContextKind {
        match self.capture {
            AskCapture::Selected(_) => AskContextKind::Selected,
            AskCapture::Caret(_) => AskContextKind::Caret,
            AskCapture::Unavailable => AskContextKind::Unavailable,
        }
    }

    pub(crate) fn selected_source(&self) -> Option<&str> {
        match &self.capture {
            AskCapture::Selected(selection) => Some(selection.text()),
            _ => None,
        }
    }
}

impl Drop for AskSession {
    fn drop(&mut self) {
        if let Some(monitor) = &self.monitor {
            monitor.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injection::test_backend::{calls, MockBackend};
    use std::sync::Mutex;

    fn dependencies(
        selected: &str,
        available: bool,
    ) -> (
        SystemTextInjector,
        Arc<Mutex<MockBackend>>,
        Arc<InputMonitor>,
    ) {
        let mock = Arc::new(Mutex::new(MockBackend::new()));
        mock.lock().unwrap().set_selected_text(selected);
        let injector =
            SystemTextInjector::with_mock(Arc::clone(&mock), InjectionOptions::default());
        let monitor = Arc::new(InputMonitor::default());
        monitor.test_set_available(available);
        (injector, mock, monitor)
    }

    #[test]
    fn edit_captures_mock_selection_and_releases_monitor_on_drop() {
        let settings = Settings::default();
        let (injector, mock, monitor) = dependencies("original", true);
        let session = EditSession::with_injector(
            &settings,
            &settings.shortcuts.edit[0],
            false,
            injector,
            Arc::clone(&monitor),
        )
        .unwrap();
        assert_eq!(session.selection.text(), "original");
        assert!(monitor.unchanged_since(session.checkpoint));
        assert_eq!(calls(&mock.lock().unwrap(), "paste"), 0);
        drop(session);
        assert!(monitor.checkpoint().is_none());
    }

    #[test]
    fn edit_refuses_no_selection_or_unavailable_monitor_without_pasting() {
        let settings = Settings::default();
        for (selected, available) in [("", true), ("original", false)] {
            let (injector, mock, monitor) = dependencies(selected, available);
            assert!(EditSession::with_injector(
                &settings,
                &settings.shortcuts.edit[0],
                false,
                injector,
                monitor
            )
            .is_err());
            assert_eq!(calls(&mock.lock().unwrap(), "paste"), 0);
        }
    }

    #[test]
    fn ask_captures_selection_or_caret_and_releases_monitor_on_drop() {
        let settings = Settings::default();
        for selected in ["original", ""] {
            let (injector, mock, monitor) = dependencies(selected, true);
            let session = AskSession::with_injector(
                &settings,
                &settings.shortcuts.ask[0],
                false,
                injector,
                Arc::clone(&monitor),
            );
            assert_eq!(
                session.kind(),
                if selected.is_empty() {
                    AskContextKind::Caret
                } else {
                    AskContextKind::Selected
                }
            );
            assert_eq!(
                session.selected_source(),
                if selected.is_empty() {
                    None
                } else {
                    Some(selected)
                }
            );
            assert!(session.checkpoint.is_some());
            assert_eq!(calls(&mock.lock().unwrap(), "paste"), 0);
            drop(session);
            assert!(monitor.checkpoint().is_none());
        }
    }

    #[test]
    fn ask_keeps_unreadable_capture_panel_only_and_does_not_fallback_to_caret() {
        let settings = Settings::default();
        let (injector, mock, monitor) = dependencies("private", true);
        mock.lock().unwrap().set_text_readable(false);
        let session = AskSession::with_injector(
            &settings,
            &settings.shortcuts.ask[0],
            false,
            injector,
            monitor,
        );
        assert_eq!(session.kind(), AskContextKind::Unavailable);
        assert!(session.monitor.is_none());
        assert!(session.checkpoint.is_none());
        assert_eq!(calls(&mock.lock().unwrap(), "paste"), 0);
    }

    #[test]
    fn ask_retains_selected_source_but_disables_caret_target_without_monitor() {
        let settings = Settings::default();
        for selected in ["original", ""] {
            let (injector, _, monitor) = dependencies(selected, false);
            let session = AskSession::with_injector(
                &settings,
                &settings.shortcuts.ask[0],
                false,
                injector,
                monitor,
            );
            assert_eq!(
                session.kind(),
                if selected.is_empty() {
                    AskContextKind::Unavailable
                } else {
                    AskContextKind::Selected
                }
            );
            assert!(session.monitor.is_none());
            assert!(session.checkpoint.is_none());
        }
    }
}
