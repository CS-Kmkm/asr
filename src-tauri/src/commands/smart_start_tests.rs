//! The Smart Dictate/Edit start over a fake selection/focus layer.
use super::*;
use crate::injection::test_backend::{calls, target, MockBackend};
use crate::smart_dictate::SmartRoute;
use std::sync::{Arc, Mutex};

fn start(
    setup: impl FnOnce(&MockBackend),
    monitor_available: bool,
) -> (Result<SmartStart, String>, Arc<Mutex<MockBackend>>) {
    let mock = Arc::new(Mutex::new(MockBackend::new()));
    setup(&mock.lock().unwrap());
    let monitor = Arc::new(InputMonitor::default());
    monitor.test_set_available(monitor_available);
    let settings = Settings::default();
    let started = smart_start_with(
        SystemTextInjector::with_mock(Arc::clone(&mock), InjectionOptions::default()),
        monitor,
        &settings.shortcuts.dictate[0],
        &settings.shortcuts.dictate,
        true,
    );
    (started, mock)
}

#[test]
fn a_selection_starts_a_monitored_edit_of_exactly_that_selection() {
    let (started, mock) = start(|backend| backend.set_selected_text("original"), true);
    let started = started.unwrap();
    assert_eq!(started.route, SmartRoute::Edit);
    let edit = started.edit.expect("edit session");
    assert_eq!(edit.selection.text(), "original");
    assert_eq!(edit.selection.target(), &target());
    assert!(edit.monitor.unchanged_since(edit.checkpoint));
    assert_eq!(started.target, Some(target()));
    assert_eq!(calls(&mock.lock().unwrap(), "paste"), 0);
}

#[test]
fn no_selection_dictates_into_the_field_read_at_the_press() {
    let (started, _mock) = start(|_| {}, true);
    let started = started.unwrap();
    assert_eq!(started.route, SmartRoute::Dictate);
    assert!(started.edit.is_none());
    assert_eq!(started.target, Some(target()));
}

#[test]
fn an_unknown_selection_copies_only_and_keeps_the_field_for_context() {
    let (started, mock) = start(
        |backend| {
            backend.set_selected_text("unseen");
            backend.set_text_readable(false);
        },
        true,
    );
    let started = started.unwrap();
    assert!(matches!(started.route, SmartRoute::CopyOnly(_)));
    assert!(started.edit.is_none());
    // Kept for app style and overlay placement only; never pasted into.
    assert_eq!(started.target, Some(target()));
    assert_eq!(calls(&mock.lock().unwrap(), "paste"), 0);
    assert_eq!(calls(&mock.lock().unwrap(), "write"), 0);
}

#[test]
fn a_selection_that_cannot_be_monitored_refuses_the_start_instead_of_dictating() {
    let (started, mock) = start(|backend| backend.set_selected_text("original"), false);
    assert_eq!(
        started.err().as_deref(),
        Some("Speak to edit could not monitor the original selection safely.")
    );
    assert_eq!(mock.lock().unwrap().content(), "prefix original suffix");
}
