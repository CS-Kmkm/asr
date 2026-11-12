//! The Smart Dictate/Edit start over a fake selection/focus layer.
use super::*;
use crate::injection::test_backend::{calls, target, MockBackend};
use crate::smart_dictate::SmartRoute;
use std::sync::{Arc, Mutex};

fn start(
    setup: impl FnOnce(&MockBackend),
    monitor_available: bool,
) -> (Result<SmartStart, String>, Arc<Mutex<MockBackend>>) {
    start_with_monitor(setup, monitor_available).0
}

/// A start, the fake field it read and the monitor it was given.
type Started = (
    (Result<SmartStart, String>, Arc<Mutex<MockBackend>>),
    Arc<InputMonitor>,
);

fn start_with_monitor(setup: impl FnOnce(&MockBackend), monitor_available: bool) -> Started {
    let mock = Arc::new(Mutex::new(MockBackend::new()));
    setup(&mock.lock().unwrap());
    let monitor = Arc::new(InputMonitor::default());
    monitor.test_set_available(monitor_available);
    let returned = Arc::clone(&monitor);
    let settings = Settings::default();
    let started = smart_start_with(
        SystemTextInjector::with_mock(Arc::clone(&mock), InjectionOptions::default()),
        monitor,
        &settings.shortcuts.dictate[0],
        &settings.shortcuts.dictate,
        true,
    );
    ((started, mock), returned)
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
    assert!(
        started.press.is_some(),
        "press-time checkpoint and field kept"
    );
    assert_eq!(started.target, Some(target()));
}

#[test]
fn input_while_the_selection_is_read_refuses_a_smart_edit() {
    let mock = Arc::new(Mutex::new(MockBackend::new()));
    mock.lock().unwrap().set_selected_text("original");
    let monitor = Arc::new(InputMonitor::default());
    monitor.test_set_available(true);
    {
        let monitor = Arc::clone(&monitor);
        mock.lock()
            .unwrap()
            .set_on_read(move || monitor.test_record_input());
    }
    let settings = Settings::default();
    let started = smart_start_with(
        SystemTextInjector::with_mock(Arc::clone(&mock), InjectionOptions::default()),
        Arc::clone(&monitor),
        &settings.shortcuts.dictate[0],
        &settings.shortcuts.dictate,
        true,
    );
    assert!(started.is_err());
    assert!(monitor.checkpoint().is_none(), "the monitor was stopped");
    assert_eq!(calls(&mock.lock().unwrap(), "paste"), 0);
}

#[test]
fn a_copy_only_start_keeps_no_monitor_running() {
    let ((started, _mock), monitor) = start_with_monitor(
        |backend| {
            backend.set_selected_text("unseen");
            backend.set_text_readable(false);
        },
        true,
    );
    assert!(started.unwrap().press.is_none());
    assert!(monitor.checkpoint().is_none());
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

#[test]
fn a_held_key_uses_its_key_down_snapshot_and_never_reads_the_field_again() {
    // Taken at key-down: a selection made during the hold delay is not seen.
    let ((started, mock), monitor) =
        start_with_monitor(|backend| backend.set_selected_text("original"), true);
    mock.lock().unwrap().set_selected_text("");
    let reads = || mock.lock().unwrap().content();
    let before = reads();
    match smart_source(true, Some(SmartPress(started))).unwrap() {
        SmartSource::Pressed(start) => {
            assert_eq!(start.route, SmartRoute::Edit);
            assert_eq!(start.edit.unwrap().selection.text(), "original");
        }
        _ => panic!("the key-down snapshot must be used"),
    }
    assert_eq!(reads(), before);
    assert!(monitor.checkpoint().is_none(), "dropped with its session");
    // A refused key-down snapshot refuses the start.
    assert!(smart_source(true, Some(SmartPress(Err("refused".into())))).is_err());
    // Without a snapshot the start reads the field itself.
    assert!(matches!(
        smart_source(true, None),
        Ok(SmartSource::ProbeNow)
    ));
    // Smart routing turned off meanwhile: the snapshot is dropped and its
    // monitor stops.
    let ((started, _mock), monitor) = start_with_monitor(|_| {}, true);
    assert!(monitor.checkpoint().is_some());
    assert!(matches!(
        smart_source(false, Some(SmartPress(started))),
        Ok(SmartSource::Off)
    ));
    assert!(monitor.checkpoint().is_none());
}
