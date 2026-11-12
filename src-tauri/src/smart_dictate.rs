//! Opt-in Smart Dictate/Edit: with the setting on, the Dictation shortcut
//! chooses Speak to edit or Dictate from the selection at the press.
//!
//! The decision is a fixed rule over one read-only probe of the original
//! target, taken when the shortcut starts the take; no model runs. The
//! routed mode is fixed for the whole take, so a selection made or changed
//! while speaking never redirects it, and every later guard of the routed
//! mode (focus, IME, input activity, selection content, protected fields)
//! still applies before anything is pasted.
//!
//! | Probe at the press                         | Route      | Delivery                         |
//! |--------------------------------------------|------------|----------------------------------|
//! | Readable, nonempty selection               | `Edit`     | Speak to edit on that selection  |
//! | Readable field, nothing selected           | `Dictate`  | Ordinary Dictate into that field |
//! | Anything else (unreadable, password field, | `CopyOnly` | Dictate, result copied only;     |
//! | this app, no target, open IME conversion)  |            | never pasted, never replaces     |
use crate::injection::{SelectionProbe, UnknownSelection};
use crate::state::PipelineMode;
use crate::types::Settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SmartRoute {
    /// Speak to edit replaces exactly the selection read at the press.
    Edit,
    /// Ordinary Dictate into the field focused at the press.
    Dictate,
    /// The selection state is unknown: the take is dictated, and its result
    /// goes to the clipboard and History only, so an unseen selection can
    /// never be replaced.
    CopyOnly(UnknownSelection),
}

impl SmartRoute {
    /// Content-free code shown on the overlay and kept with the take.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Dictate => "dictate",
            Self::CopyOnly(_) => "copy_only",
        }
    }

    /// The pipeline mode that processes the take.
    pub(crate) fn mode(self) -> PipelineMode {
        match self {
            Self::Edit => PipelineMode::Edit,
            Self::Dictate | Self::CopyOnly(_) => PipelineMode::Dictate,
        }
    }
}

/// The routing rule. Only a positively read selection becomes an Edit and
/// only a positively empty one becomes an ordinary Dictate.
pub(crate) fn decide(probe: &SelectionProbe) -> SmartRoute {
    match probe {
        SelectionProbe::Selected(_) => SmartRoute::Edit,
        SelectionProbe::Empty { .. } => SmartRoute::Dictate,
        SelectionProbe::Unknown(reason) => SmartRoute::CopyOnly(*reason),
    }
}

/// Whether a start routes by selection. Only a Dictation shortcut press
/// does: the app's own Record button, dictation tests and the other mode
/// shortcuts keep their fixed behavior, and automatic voice mode (which
/// already reads the selection) takes precedence.
pub(crate) fn applies(
    settings: &Settings,
    mode: PipelineMode,
    from_shortcut: bool,
    dictation_test: bool,
) -> bool {
    settings.smart_dictate_edit
        && !settings.automatic_voice_mode
        && mode == PipelineMode::Dictate
        && from_shortcut
        && !dictation_test
}

/// The fixed History/notice detail of a copy-only take that would otherwise
/// read as started from this app.
pub(crate) fn copy_only_detail(
    route: Option<SmartRoute>,
    detail: Option<&'static str>,
) -> Option<&'static str> {
    match (route, detail) {
        (Some(SmartRoute::CopyOnly(_)), None | Some("no_external_target")) => {
            Some(crate::injection::InsertionDetail::SelectionUnknown.code())
        }
        _ => detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injection::{
        test_backend::{calls, target, MockBackend},
        InjectionError, InjectionOptions, SystemTextInjector, TargetWindow,
    };
    use crate::input_monitor::InputMonitor;
    use crate::sessions::EditSession;
    use std::sync::{Arc, Mutex};

    fn injector(mock: &Arc<Mutex<MockBackend>>) -> SystemTextInjector {
        SystemTextInjector::with_mock(Arc::clone(mock), InjectionOptions::default())
    }

    fn mock(setup: impl FnOnce(&MockBackend)) -> Arc<Mutex<MockBackend>> {
        let mock = Arc::new(Mutex::new(MockBackend::new()));
        setup(&mock.lock().unwrap());
        mock
    }

    fn route_of(setup: impl FnOnce(&MockBackend)) -> SmartRoute {
        let mock = mock(setup);
        let route = decide(&injector(&mock).probe_selection());
        // Probing is read-only: no input, no clipboard.
        let backend = mock.lock().unwrap();
        for call in ["paste", "write", "delete", "select", "collapse", "restore"] {
            assert_eq!(calls(&backend, call), 0, "{call}");
        }
        route
    }

    fn secure_target() -> TargetWindow {
        TargetWindow {
            is_secure: true,
            ..target()
        }
    }

    fn own_target() -> TargetWindow {
        TargetWindow {
            process_id: std::process::id(),
            ..target()
        }
    }

    #[test]
    fn routing_table_covers_selected_empty_and_every_unknown_state() {
        use UnknownSelection::*;
        type Setup = Box<dyn Fn(&MockBackend)>;
        let cases: Vec<(&str, Setup, SmartRoute)> = vec![
            (
                "selection",
                Box::new(|b| b.set_selected_text("original")),
                SmartRoute::Edit,
            ),
            ("no selection", Box::new(|_| {}), SmartRoute::Dictate),
            (
                "unreadable text, selection known empty",
                Box::new(|b| b.set_text_readable(false)),
                SmartRoute::Dictate,
            ),
            (
                "unreadable text with a selection",
                Box::new(|b| {
                    b.set_selected_text("hidden");
                    b.set_text_readable(false);
                }),
                SmartRoute::CopyOnly(Unreadable),
            ),
            (
                "unreadable text and selection",
                Box::new(|b| {
                    b.set_text_readable(false);
                    b.set_selection_readable(false);
                }),
                SmartRoute::CopyOnly(Unreadable),
            ),
            (
                "password field",
                Box::new(|b| b.set_captured(Ok(secure_target()))),
                SmartRoute::CopyOnly(Protected),
            ),
            (
                "elevated target",
                Box::new(|b| b.set_captured(Err(InjectionError::PrivilegeMismatch))),
                SmartRoute::CopyOnly(Protected),
            ),
            (
                "no foreground window",
                Box::new(|b| b.set_captured(Err(InjectionError::NoForegroundWindow))),
                SmartRoute::CopyOnly(NoTarget),
            ),
            (
                "focus moved while reading",
                Box::new(|b| b.set_target_valid(false)),
                SmartRoute::CopyOnly(NoTarget),
            ),
            (
                "this app in the foreground",
                Box::new(|b| b.set_captured(Ok(own_target()))),
                SmartRoute::CopyOnly(ThisApp),
            ),
            (
                "selected embedded object without characters",
                Box::new(|b| b.set_object_selected(true)),
                SmartRoute::CopyOnly(Unreadable),
            ),
            (
                "unreadable text, embedded object selected",
                Box::new(|b| {
                    b.set_text_readable(false);
                    b.set_object_selected(true);
                }),
                SmartRoute::CopyOnly(Unreadable),
            ),
            (
                "open IME conversion with a selection",
                Box::new(|b| {
                    b.set_selected_text("変換");
                    b.ime.set(Some(true));
                }),
                SmartRoute::CopyOnly(ImeComposing),
            ),
            (
                "open IME conversion without a selection",
                Box::new(|b| b.ime.set(Some(true))),
                SmartRoute::CopyOnly(ImeComposing),
            ),
            (
                "IME state unknown, selection read",
                Box::new(|b| {
                    b.set_selected_text("original");
                    b.ime.set(None);
                }),
                SmartRoute::Edit,
            ),
        ];
        for (name, setup, expected) in cases {
            assert_eq!(route_of(setup), expected, "{name}");
        }
    }

    #[test]
    fn routes_map_to_existing_modes_and_fixed_codes() {
        assert_eq!(SmartRoute::Edit.mode(), PipelineMode::Edit);
        assert_eq!(SmartRoute::Dictate.mode(), PipelineMode::Dictate);
        let copy = SmartRoute::CopyOnly(UnknownSelection::Unreadable);
        assert_eq!(copy.mode(), PipelineMode::Dictate);
        assert_eq!(
            [
                SmartRoute::Edit.code(),
                SmartRoute::Dictate.code(),
                copy.code()
            ],
            ["edit", "dictate", "copy_only"]
        );
    }

    #[test]
    fn only_an_opted_in_dictation_shortcut_press_routes() {
        let mut settings = Settings::default();
        assert!(!settings.smart_dictate_edit, "Smart routing is opt-in");
        assert!(!applies(&settings, PipelineMode::Dictate, true, false));
        settings.smart_dictate_edit = true;
        assert!(applies(&settings, PipelineMode::Dictate, true, false));
        for mode in [
            PipelineMode::Edit,
            PipelineMode::Translate,
            PipelineMode::Ask,
        ] {
            assert!(!applies(&settings, mode, true, false), "{mode:?}");
        }
        // The Record button and dictation tests keep plain Dictate.
        assert!(!applies(&settings, PipelineMode::Dictate, false, false));
        assert!(!applies(&settings, PipelineMode::Dictate, true, true));
        // Automatic voice mode takes precedence.
        settings.automatic_voice_mode = true;
        assert!(!applies(&settings, PipelineMode::Dictate, true, false));
    }

    #[test]
    fn copy_only_takes_name_the_unknown_selection_instead_of_this_app() {
        let copy = Some(SmartRoute::CopyOnly(UnknownSelection::Unreadable));
        assert_eq!(
            copy_only_detail(copy, Some("no_external_target")),
            Some("selection_unknown")
        );
        assert_eq!(copy_only_detail(copy, None), Some("selection_unknown"));
        assert_eq!(
            copy_only_detail(copy, Some("clipboard_unavailable")),
            Some("clipboard_unavailable")
        );
        for route in [None, Some(SmartRoute::Dictate), Some(SmartRoute::Edit)] {
            assert_eq!(
                copy_only_detail(route, Some("no_external_target")),
                Some("no_external_target")
            );
            assert_eq!(copy_only_detail(route, None), None);
        }
    }

    /// An Edit-routed take over the probed selection, as the start builds it.
    fn routed_edit(
        setup: impl FnOnce(&MockBackend),
    ) -> (EditSession, Arc<Mutex<MockBackend>>, Arc<InputMonitor>) {
        let mock = mock(|backend| {
            backend.set_selected_text("original");
            setup(backend);
        });
        let injector = injector(&mock);
        let SelectionProbe::Selected(selection) = injector.probe_selection() else {
            panic!("selection expected");
        };
        let monitor = Arc::new(InputMonitor::default());
        monitor.test_set_available(true);
        let checkpoint = monitor.checkpoint().unwrap();
        let session = EditSession::from_monitored(
            selection,
            injector,
            Arc::clone(&monitor),
            checkpoint,
            true,
        )
        .expect("monitored edit session");
        (session, mock, monitor)
    }

    fn replace(session: &EditSession) -> Result<crate::injection::InsertResult, InjectionError> {
        session.replace("edited")
    }

    #[test]
    fn a_routed_edit_replaces_only_the_unchanged_original_selection() {
        let (session, mock, _monitor) = routed_edit(|_| {});
        assert_eq!(
            replace(&session),
            Ok(crate::injection::InsertResult::ClipboardPaste)
        );
        assert_eq!(mock.lock().unwrap().content(), "prefix edited suffix");
    }

    #[test]
    fn a_routed_edit_never_replaces_after_the_target_changed() {
        type Change = Box<dyn Fn(&MockBackend, &InputMonitor)>;
        let changes: Vec<(&str, Change)> = vec![
            (
                "selection changed",
                Box::new(|b, _| b.set_selected_text("other")),
            ),
            ("focus moved", Box::new(|b, _| b.set_target_valid(false))),
            ("IME composing", Box::new(|b, _| b.ime.set(Some(true)))),
            ("text typed meanwhile", Box::new(|b, _| b.type_text("x"))),
            (
                "keystrokes observed meanwhile",
                Box::new(|_, monitor| monitor.test_record_input()),
            ),
            (
                "another field with the same text and selection focused",
                Box::new(|b, _| b.set_identity(vec![9])),
            ),
            (
                "target unreadable",
                Box::new(|b, _| b.set_text_readable(false)),
            ),
        ];
        for (name, change) in changes {
            let (session, mock, monitor) = routed_edit(|_| {});
            change(&mock.lock().unwrap(), &monitor);
            let result = replace(&session);
            let backend = mock.lock().unwrap();
            assert_eq!(calls(&backend, "paste"), 0, "{name}: {result:?}");
            assert!(
                !backend.content().contains("edited"),
                "{name}: the field was edited"
            );
            // The edit can still be recovered from the clipboard.
            drop(backend);
            session.injector.copy_to_clipboard("edited").unwrap();
        }
    }

    #[test]
    fn a_routed_edit_needs_a_monitor_and_releases_it() {
        let mock = mock(|backend| backend.set_selected_text("original"));
        let injector = injector(&mock);
        let SelectionProbe::Selected(selection) = injector.probe_selection() else {
            panic!("selection expected");
        };
        let monitor = Arc::new(InputMonitor::default());
        monitor.test_set_available(true);
        let checkpoint = monitor.checkpoint().unwrap();
        // Input observed while the selection was read refuses the edit.
        monitor.test_record_input();
        assert!(EditSession::from_monitored(
            selection,
            injector,
            Arc::clone(&monitor),
            checkpoint,
            true
        )
        .is_err());
        assert!(monitor.checkpoint().is_none(), "the monitor was stopped");
        let settings = Settings::default();
        let unavailable = InputMonitor::default();
        assert!(EditSession::start_monitor(
            &unavailable,
            &settings.shortcuts.dictate[0],
            &settings.shortcuts.dictate,
            true,
        )
        .is_err());
        let (session, _mock, monitor) = routed_edit(|_| {});
        assert!(monitor.checkpoint().is_some());
        drop(session);
        assert!(monitor.checkpoint().is_none());
    }
}
