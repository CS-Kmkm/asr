use crate::{
    parse_shortcut,
    state::{PipelineMode, PipelinePhase},
    types::Settings,
};
use tauri_plugin_global_shortcut::{Code, Shortcut};

/// Short-lived capture of an already registered chord inside the main window.
/// Chords stay registered, so the OS can deliver them to the recorder rather
/// than starting a voice operation. A stale component cannot cancel a new lease.
#[derive(Default)]
pub(crate) struct CaptureLease {
    current: Option<(u64, std::time::Instant)>,
    newest: u64,
}

impl CaptureLease {
    pub(crate) fn begin(&mut self, id: u64) {
        if id <= self.newest {
            return;
        }
        self.newest = id;
        self.current = Some((
            id,
            std::time::Instant::now() + std::time::Duration::from_secs(15),
        ));
    }
    pub(crate) fn end(&mut self, id: u64) {
        if self.current.is_some_and(|(current, _)| current == id) {
            self.current = None;
        }
    }
    pub(crate) fn take(&mut self, focused: bool) -> Option<u64> {
        self.current
            .take()
            .filter(|(_, expires)| focused && std::time::Instant::now() < *expires)
            .map(|(id, _)| id)
    }
}

/// What a voice-mode hotkey press does in the current lifecycle phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Press {
    Start,
    Stop,
    /// A second press while a start waits (for example for the speech
    /// model) cancels it, so the microphone never opens unexpectedly later.
    CancelStart,
    /// Another mode is recording; the press is reported and changes nothing.
    Busy(&'static str),
    /// Processing is already visible on the overlay and finishes by itself.
    Ignore,
}

pub(crate) fn press(
    phase: PipelinePhase,
    active: Option<PipelineMode>,
    requested: PipelineMode,
) -> Press {
    match phase {
        PipelinePhase::Idle => Press::Start,
        PipelinePhase::Recording if active == Some(requested) => Press::Stop,
        PipelinePhase::Recording => Press::Busy("another voice mode is already recording"),
        PipelinePhase::Starting => Press::CancelStart,
        PipelinePhase::Processing => Press::Ignore,
    }
}

/// The chord that cancels the active voice session: Esc, with no modifier.
pub(crate) fn cancel_chord() -> Shortcut {
    Shortcut::new(None, Code::Escape)
}

/// Whether the cancel chord should be registered. A global hotkey takes the
/// key from every app, so Esc is held only while a session is starting,
/// recording, or processing. A route the user assigned to Esc wins: the app
/// never takes a chord the user chose, so Esc keeps running that route and
/// the overlay's cancel button is the cancel path. Settings warns about this
/// on any route that uses Esc (`isCancelKeyChord` in `SettingsPage.tsx`).
pub(crate) fn wants_cancel_chord(phase: PipelinePhase, routes: &Routes) -> bool {
    phase != PipelinePhase::Idle && routes.find(cancel_chord()).is_none()
}

/// Brings the cancel chord registration in line with `wanted`. A refused
/// registration (another app holds Esc) stays unregistered and is retried
/// at the next phase change; a failed removal is retried the same way.
pub(crate) fn sync_cancel_chord<E: std::fmt::Display>(
    registered: &mut bool,
    wanted: bool,
    register: impl FnOnce(Shortcut) -> Result<(), E>,
    unregister: impl FnOnce(Shortcut) -> Result<(), E>,
) {
    if wanted == *registered {
        return;
    }
    let result = if wanted {
        register(cancel_chord())
    } else {
        unregister(cancel_chord())
    };
    match result {
        Ok(()) => *registered = wanted,
        Err(error) => eprintln!("Cancel shortcut (Esc) update failed: {error}"),
    }
}

/// `update_registrations` for a settings save, keeping the Esc cancel chord
/// consistent with the routes. Esc is released first when the new routes
/// take it (a route registration would otherwise fail on our own hold), and
/// afterwards follows the routes in effect: the new ones, or the old ones
/// after a rollback. A route that gave Esc up during a session hands it back
/// to the cancel chord.
pub(crate) fn update_registrations_with_cancel_chord<E: std::fmt::Display>(
    cancel_chord: &mut bool,
    phase: PipelinePhase,
    old: &Routes,
    new: &Routes,
    mut register: impl FnMut(Shortcut) -> Result<(), E>,
    mut unregister: impl FnMut(Shortcut) -> Result<(), E>,
    persist: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    sync_cancel_chord(
        cancel_chord,
        wants_cancel_chord(phase, old) && wants_cancel_chord(phase, new),
        &mut register,
        &mut unregister,
    );
    let result = update_registrations(old, new, &mut register, &mut unregister, persist);
    let routes = if result.is_ok() { new } else { old };
    sync_cancel_chord(
        cancel_chord,
        wants_cancel_chord(phase, routes),
        &mut register,
        &mut unregister,
    );
    result
}

/// Routed actions in collision precedence order: when saved chords collide,
/// the older action keeps the chord (Dictate, selected-text Translate, voice
/// Translate, Speak to edit, then Ask).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Dictate,
    SelectedText,
    Translate,
    Edit,
    Ask,
}

impl Action {
    /// English name used in shortcut errors and warnings. The frontend
    /// localizes these names, so keep them in sync with `src/i18n.tsx`.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Dictate => "Dictation",
            Self::SelectedText => "Selected-text translation",
            Self::Translate => "Voice Translate",
            Self::Edit => "Speak to edit",
            Self::Ask => "Ask Anything",
        }
    }
}

fn describe(action: Action, text: &str) -> String {
    format!("{} ({})", action.label(), text.trim())
}

#[derive(Clone)]
pub(crate) struct Route {
    pub(crate) chord: Shortcut,
    pub(crate) text: String,
    pub(crate) action: Action,
}

impl Route {
    /// Names the action and the chord as the user typed it, for example
    /// `Voice Translate (Ctrl+Shift+Y)`, instead of a debug `HotKey { .. }`.
    pub(crate) fn describe(&self) -> String {
        describe(self.action, &self.text)
    }
}

#[derive(Clone)]
pub(crate) struct Routes(pub(crate) Vec<Route>);

impl Routes {
    #[cfg(test)]
    pub(crate) fn parse(settings: &Settings) -> Result<Self, String> {
        Self::parse_with_collisions(settings, false).map(|(routes, _)| routes)
    }

    /// Existing settings may predate a mode and therefore reuse its default
    /// chord. Keep the higher-precedence route active until the user resolves
    /// the overlap.
    pub(crate) fn parse_saved(settings: &Settings) -> Result<Self, String> {
        Self::parse_saved_with_shadowed(settings).map(|(routes, _)| routes)
    }

    /// Like `parse_saved`, also returning the routes that lost a collision and
    /// are therefore not dispatched until the user reassigns them.
    pub(crate) fn parse_saved_with_shadowed(
        settings: &Settings,
    ) -> Result<(Self, Vec<Route>), String> {
        Self::parse_with_collisions(settings, true)
    }

    fn parse_with_collisions(
        settings: &Settings,
        allow_collisions: bool,
    ) -> Result<(Self, Vec<Route>), String> {
        let mut routes = Vec::new();
        let mut shadowed = Vec::new();
        for (action, values) in [
            (Action::Dictate, settings.shortcuts.dictate.as_slice()),
            (
                Action::SelectedText,
                std::slice::from_ref(&settings.translation_hotkey),
            ),
            (Action::Translate, settings.shortcuts.translate.as_slice()),
            (Action::Edit, settings.shortcuts.edit.as_slice()),
            (Action::Ask, settings.shortcuts.ask.as_slice()),
        ] {
            if !(1..=4).contains(&values.len()) {
                return Err(format!(
                    "each voice mode requires one to four shortcuts: {}",
                    action.label()
                ));
            }
            for text in values {
                Self::push(&mut routes, &mut shadowed, action, text, allow_collisions)?;
            }
        }
        Ok((Self(routes), shadowed))
    }

    fn push(
        routes: &mut Vec<Route>,
        shadowed: &mut Vec<Route>,
        action: Action,
        text: &str,
        allow_collisions: bool,
    ) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err(format!("shortcut cannot be empty: {}", action.label()));
        }
        let chord =
            parse_shortcut(text).map_err(|error| format!("{error}: {}", describe(action, text)))?;
        let route = Route {
            chord,
            text: text.to_owned(),
            action,
        };
        if let Some(existing) = routes.iter().find(|existing| existing.chord == chord) {
            if !allow_collisions {
                return Err(format!(
                    "shortcuts must be unique across all actions: {}, {}",
                    existing.describe(),
                    route.describe()
                ));
            }
            shadowed.push(route);
            return Ok(());
        }
        routes.push(route);
        Ok(())
    }

    pub(crate) fn find(&self, chord: Shortcut) -> Option<&Route> {
        self.0.iter().find(|route| route.chord == chord)
    }

    #[cfg(test)]
    pub(crate) fn chords(&self) -> Vec<Shortcut> {
        self.0.iter().map(|route| route.chord).collect()
    }
}

/// Keep only chords the OS accepted. Saved settings may include occupied
/// chords; they must not be treated as registered during the next update.
pub(crate) fn register_available<E>(
    routes: &Routes,
    mut register: impl FnMut(Shortcut) -> Result<(), E>,
) -> (Routes, Vec<(Route, E)>) {
    let mut active = Vec::new();
    let mut failed = Vec::new();
    for route in &routes.0 {
        match register(route.chord) {
            Ok(()) => active.push(route.clone()),
            Err(error) => failed.push((route.clone(), error)),
        }
    }
    (Routes(active), failed)
}

pub(crate) fn desired_routes_for_update(
    previous: &Settings,
    candidate: &Settings,
    active: &Routes,
) -> Result<Routes, String> {
    if candidate.shortcuts == previous.shortcuts
        && candidate.translation_hotkey == previous.translation_hotkey
    {
        Ok(active.clone())
    } else {
        let (desired, proposed_shadowed) = Routes::parse_saved_with_shadowed(candidate)?;
        let (previous_owners, previous_shadowed) = Routes::parse_saved_with_shadowed(previous)?;
        for loser in proposed_shadowed {
            let owner = desired
                .find(loser.chord)
                .expect("a shadowed chord has an owner");
            let unchanged = previous_shadowed
                .iter()
                .any(|old| old.action == loser.action && old.chord == loser.chord)
                && previous_owners
                    .find(loser.chord)
                    .is_some_and(|old_owner| old_owner.action == owner.action);
            if !unchanged {
                return Err(format!(
                    "shortcuts must be unique across all actions: {}, {}",
                    owner.describe(),
                    loser.describe()
                ));
            }
        }
        let mut desired = desired;
        // A chord already saved for the same action but unavailable at
        // startup remains a warning, not a reason an unrelated chord edit
        // fails. A genuinely new chord must still register successfully.
        desired.0.retain(|route| {
            active.find(route.chord).is_some()
                || previous_owners
                    .find(route.chord)
                    .is_none_or(|old| old.action != route.action)
        });
        Ok(desired)
    }
}

pub(crate) fn inactive_descriptions(
    settings: &Settings,
    active: &Routes,
) -> Result<Vec<String>, String> {
    let (saved, shadowed) = Routes::parse_saved_with_shadowed(settings)?;
    Ok(shadowed
        .iter()
        .chain(
            saved
                .0
                .iter()
                .filter(|route| active.find(route.chord).is_none()),
        )
        .map(Route::describe)
        .collect())
}

/// Registration changes are applied before persistence; routing is published by
/// the caller only after this returns. A failed rollback requires a restart.
/// Errors name the affected action and chord; the plugin's own error text
/// (which prints a debug `HotKey { .. }`) is only logged.
pub(crate) fn update_registrations<E: std::fmt::Display>(
    old: &Routes,
    new: &Routes,
    mut register: impl FnMut(Shortcut) -> Result<(), E>,
    mut unregister: impl FnMut(Shortcut) -> Result<(), E>,
    mut persist: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let removed: Vec<&Route> = old
        .0
        .iter()
        .filter(|route| new.find(route.chord).is_none())
        .collect();
    let added: Vec<&Route> = new
        .0
        .iter()
        .filter(|route| old.find(route.chord).is_none())
        .collect();
    let mut removed_done = Vec::new();
    let mut added_done = Vec::new();
    let mut failure = None;
    for route in &added {
        match register(route.chord) {
            Ok(()) => added_done.push(*route),
            Err(error) => {
                eprintln!("Shortcut registration failed for {}: {error}", route.text);
                failure = Some(format!(
                    "shortcut registration failed: {}",
                    route.describe()
                ));
                break;
            }
        }
    }
    if failure.is_none() {
        for route in &removed {
            match unregister(route.chord) {
                Ok(()) => removed_done.push(*route),
                Err(error) => {
                    eprintln!("Shortcut removal failed for {}: {error}", route.text);
                    failure = Some(format!("shortcut removal failed: {}", route.describe()));
                    break;
                }
            }
        }
    }
    if failure.is_none() {
        failure = persist().err();
    }
    let Some(failure) = failure else {
        return Ok(());
    };
    let mut unrestored = Vec::new();
    for route in removed_done {
        if let Err(error) = register(route.chord) {
            eprintln!("Shortcut restoration failed for {}: {error}", route.text);
            unrestored.push(route.describe());
        }
    }
    for route in added_done.into_iter().rev() {
        if let Err(error) = unregister(route.chord) {
            eprintln!("Shortcut restoration failed for {}: {error}", route.text);
            unrestored.push(route.describe());
        }
    }
    if unrestored.is_empty() {
        Err(failure)
    } else {
        Err(format!(
            "{failure}; shortcut restoration failed, restart required: {}",
            unrestored.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_leases_ignore_stale_cancellation_and_never_capture_outside_main_window() {
        let mut capture = CaptureLease::default();
        capture.begin(1);
        capture.begin(2);
        capture.begin(1); // A delayed older IPC request cannot replace B.
        capture.end(1);
        assert_eq!(capture.take(true), Some(2));
        assert_eq!(capture.take(true), None);
        capture.begin(3);
        assert_eq!(capture.take(false), None);
        assert_eq!(capture.take(true), None);
        capture.current = Some((
            4,
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        ));
        assert_eq!(capture.take(true), None);
    }
    use std::{cell::RefCell, collections::HashSet};

    #[test]
    fn esc_is_held_only_while_a_voice_session_is_active() {
        let routes = Routes::parse(&Settings::default()).unwrap();
        assert!(!wants_cancel_chord(PipelinePhase::Idle, &routes));
        for active in [
            PipelinePhase::Starting,
            PipelinePhase::Recording,
            PipelinePhase::Processing,
        ] {
            assert!(wants_cancel_chord(active, &routes));
        }
        // A route the user assigned to Esc keeps it, under any spelling the
        // Settings warning recognizes; Esc with a modifier is another chord.
        for spelling in ["Escape", "Esc", " esc "] {
            let mut settings = Settings::default();
            settings.shortcuts.ask[0] = spelling.into();
            let routes = Routes::parse(&settings).unwrap();
            assert!(routes.find(cancel_chord()).is_some(), "{spelling}");
            assert!(!wants_cancel_chord(PipelinePhase::Recording, &routes));
        }
        let mut settings = Settings {
            translation_hotkey: "Escape".into(),
            ..Settings::default()
        };
        let routes = Routes::parse(&settings).unwrap();
        assert!(!wants_cancel_chord(PipelinePhase::Starting, &routes));
        settings.translation_hotkey = "Ctrl+Escape".into();
        let routes = Routes::parse(&settings).unwrap();
        assert!(wants_cancel_chord(PipelinePhase::Recording, &routes));
    }

    #[test]
    fn cancel_chord_registration_follows_the_session_and_retries_refusals() {
        let calls = RefCell::new(Vec::new());
        let register = |result: Result<(), &'static str>| {
            let calls = &calls;
            move |chord: Shortcut| {
                assert_eq!(chord, cancel_chord());
                calls.borrow_mut().push("register");
                result
            }
        };
        let unregister = |chord: Shortcut| {
            assert_eq!(chord, cancel_chord());
            calls.borrow_mut().push("unregister");
            Ok::<(), &'static str>(())
        };
        let mut registered = false;
        // Idle: nothing to do.
        sync_cancel_chord(&mut registered, false, register(Ok(())), unregister);
        assert!(calls.borrow().is_empty());
        // Another app holds Esc: stays unregistered, retried next change.
        sync_cancel_chord(&mut registered, true, register(Err("taken")), unregister);
        assert!(!registered);
        sync_cancel_chord(&mut registered, true, register(Ok(())), unregister);
        assert!(registered);
        // Starting -> Recording keeps the one registration.
        sync_cancel_chord(&mut registered, true, register(Ok(())), unregister);
        sync_cancel_chord(&mut registered, false, register(Ok(())), unregister);
        assert!(!registered);
        assert_eq!(*calls.borrow(), vec!["register", "register", "unregister"]);
    }

    /// A registry that, like the OS, refuses a chord registered twice.
    struct Registry(RefCell<HashSet<Shortcut>>);

    impl Registry {
        fn register(&self, chord: Shortcut) -> Result<(), &'static str> {
            self.0
                .borrow_mut()
                .insert(chord)
                .then_some(())
                .ok_or("already registered")
        }

        fn unregister(&self, chord: Shortcut) -> Result<(), &'static str> {
            self.0
                .borrow_mut()
                .remove(&chord)
                .then_some(())
                .ok_or("not registered")
        }

        fn holds(&self, chord: Shortcut) -> bool {
            self.0.borrow().contains(&chord)
        }
    }

    fn with_esc_on_ask() -> Settings {
        let mut settings = Settings::default();
        settings.shortcuts.ask[0] = "Escape".into();
        settings
    }

    fn save(
        registry: &Registry,
        cancel_chord: &mut bool,
        phase: PipelinePhase,
        old: &Routes,
        new: &Routes,
        persisted: Result<(), String>,
    ) -> Result<(), String> {
        let mut persisted = Some(persisted);
        update_registrations_with_cancel_chord(
            cancel_chord,
            phase,
            old,
            new,
            |chord| registry.register(chord),
            |chord| registry.unregister(chord),
            || persisted.take().unwrap(),
        )
    }

    #[test]
    fn a_route_can_take_esc_from_the_cancel_chord_mid_session() {
        let old = Routes::parse(&Settings::default()).unwrap();
        let new = Routes::parse(&with_esc_on_ask()).unwrap();
        let registry = Registry(RefCell::new(old.chords().into_iter().collect()));
        registry.register(cancel_chord()).unwrap();
        let mut cancel = true;
        save(
            &registry,
            &mut cancel,
            PipelinePhase::Recording,
            &old,
            &new,
            Ok(()),
        )
        .unwrap();
        // Esc is now the Ask route's; the cancel chord let go of it.
        assert!(!cancel);
        assert!(registry.holds(cancel_chord()));
        assert_eq!(
            new.find(cancel_chord()).map(|route| route.action),
            Some(Action::Ask)
        );
    }

    #[test]
    fn removing_an_esc_route_mid_session_hands_esc_back_to_cancel() {
        let old = Routes::parse(&with_esc_on_ask()).unwrap();
        let new = Routes::parse(&Settings::default()).unwrap();
        let registry = Registry(RefCell::new(old.chords().into_iter().collect()));
        let mut cancel = false;
        save(
            &registry,
            &mut cancel,
            PipelinePhase::Processing,
            &old,
            &new,
            Ok(()),
        )
        .unwrap();
        assert!(cancel);
        assert!(registry.holds(cancel_chord()));
        // While idle the removed route simply frees Esc.
        let registry = Registry(RefCell::new(old.chords().into_iter().collect()));
        let mut cancel = false;
        save(
            &registry,
            &mut cancel,
            PipelinePhase::Idle,
            &old,
            &new,
            Ok(()),
        )
        .unwrap();
        assert!(!cancel);
        assert!(!registry.holds(cancel_chord()));
    }

    #[test]
    fn a_failed_save_restores_the_cancel_chord_with_the_old_routes() {
        let old = Routes::parse(&Settings::default()).unwrap();
        let new = Routes::parse(&with_esc_on_ask()).unwrap();
        let registry = Registry(RefCell::new(old.chords().into_iter().collect()));
        registry.register(cancel_chord()).unwrap();
        let mut cancel = true;
        assert_eq!(
            save(
                &registry,
                &mut cancel,
                PipelinePhase::Recording,
                &old,
                &new,
                Err("write failed".into()),
            ),
            Err("write failed".into())
        );
        assert!(cancel);
        assert!(registry.holds(cancel_chord()));
        assert_eq!(
            *registry.0.borrow(),
            old.chords()
                .into_iter()
                .chain([cancel_chord()])
                .collect::<HashSet<_>>()
        );
    }

    #[test]
    fn a_second_press_while_starting_cancels_instead_of_being_ignored() {
        use PipelineMode::{Dictate, Translate};
        assert_eq!(press(PipelinePhase::Idle, None, Dictate), Press::Start);
        assert_eq!(
            press(PipelinePhase::Starting, Some(Dictate), Dictate),
            Press::CancelStart
        );
        // Any voice hotkey cancels a pending start: nothing should begin
        // recording minutes later because a model was still loading.
        assert_eq!(
            press(PipelinePhase::Starting, Some(Dictate), Translate),
            Press::CancelStart
        );
        assert_eq!(
            press(PipelinePhase::Recording, Some(Dictate), Dictate),
            Press::Stop
        );
        assert_eq!(
            press(PipelinePhase::Recording, Some(Dictate), Translate),
            Press::Busy("another voice mode is already recording")
        );
        assert_eq!(
            press(PipelinePhase::Processing, Some(Dictate), Dictate),
            Press::Ignore
        );
    }

    #[test]
    fn detects_cross_action_collisions_and_chord_swaps() {
        let old = Routes::parse(&Settings::default()).unwrap();
        let mut settings = Settings::default();
        settings.shortcuts.ask[0] = settings.shortcuts.edit[0].clone();
        assert!(Routes::parse(&settings).is_err());
        settings.shortcuts.ask[0] = settings.translation_hotkey.clone();
        assert!(Routes::parse(&settings).is_err());
        settings.shortcuts.ask[0] = "Ctrl+Shift+E".into();
        settings.shortcuts.edit[0] = "Ctrl+Shift+A".into();
        let new = Routes::parse(&settings).unwrap();
        assert_eq!(
            old.chords().into_iter().collect::<HashSet<_>>(),
            new.chords().into_iter().collect()
        );
        assert_eq!(
            new.find(parse_shortcut("Ctrl+Shift+E").unwrap())
                .unwrap()
                .action,
            Action::Ask
        );
        let operations = RefCell::new(0);
        update_registrations(
            &old,
            &new,
            |_| {
                *operations.borrow_mut() += 1;
                Ok::<_, &str>(())
            },
            |_| {
                *operations.borrow_mut() += 1;
                Ok::<_, &str>(())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(*operations.borrow(), 0);
    }

    #[test]
    fn saved_collision_keeps_first_route_but_new_settings_reject_it() {
        let mut settings = Settings::default();
        settings.shortcuts.dictate[0] = settings.translation_hotkey.clone();
        assert!(Routes::parse(&settings).is_err());
        let routes = Routes::parse_saved(&settings).unwrap();
        assert_eq!(routes.0.len(), 4);
        assert_eq!(
            routes
                .find(parse_shortcut(&settings.translation_hotkey).unwrap())
                .unwrap()
                .action,
            Action::Dictate
        );
    }

    #[test]
    fn saved_collisions_keep_the_older_action_and_name_the_loser() {
        let action_for = |routes: &Routes, chord: &str| {
            routes
                .find(parse_shortcut(chord).unwrap())
                .map(|route| route.action)
        };
        // A legacy selected-text chord equal to the voice Translate default.
        let mut settings = Settings::default();
        settings.translation_hotkey = settings.shortcuts.translate[0].clone();
        let (routes, shadowed) = Routes::parse_saved_with_shadowed(&settings).unwrap();
        assert_eq!(
            action_for(&routes, "Ctrl+Shift+Y"),
            Some(Action::SelectedText)
        );
        assert_eq!(shadowed.len(), 1);
        assert_eq!(shadowed[0].describe(), "Voice Translate (Ctrl+Shift+Y)");

        for (winner, loser) in [
            (Action::Translate, Action::Edit),
            (Action::Edit, Action::Ask),
            (Action::Translate, Action::Ask),
            (Action::Dictate, Action::Ask),
        ] {
            let mut settings = Settings::default();
            let chord = "Ctrl+Alt+K".to_string();
            for action in [winner, loser] {
                match action {
                    Action::Dictate => settings.shortcuts.dictate[0] = chord.clone(),
                    Action::Translate => settings.shortcuts.translate[0] = chord.clone(),
                    Action::Edit => settings.shortcuts.edit[0] = chord.clone(),
                    Action::Ask => settings.shortcuts.ask[0] = chord.clone(),
                    Action::SelectedText => settings.translation_hotkey = chord.clone(),
                }
            }
            let (routes, shadowed) = Routes::parse_saved_with_shadowed(&settings).unwrap();
            assert_eq!(action_for(&routes, &chord), Some(winner));
            assert_eq!(shadowed.len(), 1);
            assert_eq!(shadowed[0].action, loser);
            let error = Routes::parse(&settings).err().unwrap();
            assert!(error.starts_with("shortcuts must be unique across all actions: "));
            assert!(error.contains(&format!("{} (Ctrl+Alt+K)", winner.label())));
            assert!(error.contains(&format!("{} (Ctrl+Alt+K)", loser.label())));
        }
    }

    #[test]
    fn shortcut_errors_name_the_action_instead_of_the_plugin_hotkey() {
        let old = Routes::parse(&Settings::default()).unwrap();
        let mut settings = Settings::default();
        settings.shortcuts.translate = vec!["Ctrl+Alt+Y".into()];
        let new = Routes::parse(&settings).unwrap();
        let plugin_error = "HotKey already registered: HotKey { mods: Modifiers(CONTROL | ALT), key: KeyY, id: 1 }";
        let error = update_registrations(&old, &new, |_| Err(plugin_error), |_| Ok(()), || Ok(()))
            .unwrap_err();
        assert_eq!(
            error,
            "shortcut registration failed: Voice Translate (Ctrl+Alt+Y)"
        );

        // Persistence fails and the removed chord cannot be restored.
        let removed = parse_shortcut("Ctrl+Shift+Y").unwrap();
        let error = update_registrations(
            &old,
            &new,
            |chord| {
                if chord == removed {
                    Err(plugin_error)
                } else {
                    Ok(())
                }
            },
            |_| Ok(()),
            || Err("write failed".into()),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "write failed; shortcut restoration failed, restart required: Voice Translate (Ctrl+Shift+Y)"
        );
        assert!(!error.contains("HotKey"));

        let mut settings = Settings::default();
        settings.shortcuts.ask[0] = "Ctrl+Shit+A".into();
        assert_eq!(
            Routes::parse(&settings).err().unwrap(),
            "hotkey is invalid: Ask Anything (Ctrl+Shit+A)"
        );
        settings.shortcuts.ask[0] = " ".into();
        assert_eq!(
            Routes::parse(&settings).err().unwrap(),
            "shortcut cannot be empty: Ask Anything"
        );
        settings.shortcuts.ask.clear();
        assert_eq!(
            Routes::parse(&settings).err().unwrap(),
            "each voice mode requires one to four shortcuts: Ask Anything"
        );
    }

    #[test]
    fn occupied_startup_chord_is_not_unregistered_during_repair() {
        let old = Routes::parse(&Settings::default()).unwrap();
        let occupied = parse_shortcut("Ctrl+Shift+Space").unwrap();
        let (active, failures) = register_available(&old, |chord| {
            if chord == occupied {
                Err("occupied")
            } else {
                Ok(())
            }
        });
        assert_eq!(failures.len(), 1);
        assert!(active.find(occupied).is_none());
        let mut settings = Settings::default();
        settings.shortcuts.dictate[0] = "Ctrl+Alt+V".into();
        let new = Routes::parse(&settings).unwrap();
        let removed = RefCell::new(Vec::new());
        update_registrations(
            &active,
            &new,
            |_| Ok::<_, &str>(()),
            |chord| {
                removed.borrow_mut().push(chord);
                Ok::<_, &str>(())
            },
            || Ok(()),
        )
        .unwrap();
        assert!(!removed.borrow().contains(&occupied));
    }

    #[test]
    fn unrelated_settings_update_does_not_retry_occupied_chord() {
        let previous = Settings::default();
        let occupied = parse_shortcut("Ctrl+Shift+Space").unwrap();
        let (active, _) = register_available(&Routes::parse(&previous).unwrap(), |chord| {
            if chord == occupied {
                Err("occupied")
            } else {
                Ok(())
            }
        });
        let mut candidate = previous.clone();
        candidate.interaction_sounds = true;
        let desired = desired_routes_for_update(&previous, &candidate, &active).unwrap();
        assert_eq!(desired.chords(), active.chords());
    }

    #[test]
    fn another_shortcut_can_change_without_rejecting_an_unchanged_legacy_collision() {
        let mut previous = Settings::default();
        previous.translation_hotkey = previous.shortcuts.translate[0].clone();
        let active = Routes::parse_saved(&previous).unwrap();
        let mut candidate = previous.clone();
        candidate.shortcuts.ask[0] = "Ctrl+Alt+A".into();
        let desired = desired_routes_for_update(&previous, &candidate, &active).unwrap();
        assert_eq!(desired.0.len(), active.0.len());
        assert_eq!(
            desired
                .find(parse_shortcut(&previous.translation_hotkey).unwrap())
                .unwrap()
                .action,
            Action::SelectedText
        );

        candidate.shortcuts.edit[0] = candidate.shortcuts.ask[0].clone();
        let error = desired_routes_for_update(&previous, &candidate, &active)
            .err()
            .unwrap();
        assert!(error.contains("Speak to edit"));
        assert!(error.contains("Ask Anything"));
    }

    #[test]
    fn another_shortcut_edit_keeps_an_unchanged_unavailable_chord_as_a_warning() {
        let previous = Settings::default();
        let occupied = parse_shortcut(&previous.shortcuts.dictate[0]).unwrap();
        let (active, _) = register_available(&Routes::parse(&previous).unwrap(), |chord| {
            if chord == occupied {
                Err("occupied")
            } else {
                Ok(())
            }
        });
        let mut candidate = previous.clone();
        candidate.shortcuts.ask[0] = "Ctrl+Alt+A".into();
        let desired = desired_routes_for_update(&previous, &candidate, &active).unwrap();
        assert!(desired.find(occupied).is_none());
        assert!(inactive_descriptions(&candidate, &desired)
            .unwrap()
            .iter()
            .any(|label| label.starts_with("Dictation (")));
    }

    #[test]
    fn registration_failure_restores_previous_set() {
        let old = Routes::parse(&Settings::default()).unwrap();
        let mut settings = Settings::default();
        settings.shortcuts.dictate = vec!["Ctrl+Alt+V".into()];
        let new = Routes::parse(&settings).unwrap();
        let registrations = RefCell::new(old.chords().into_iter().collect::<HashSet<_>>());
        let failed = new.0[0].chord;
        let result = update_registrations(
            &old,
            &new,
            |chord| {
                if chord == failed {
                    assert_eq!(*registrations.borrow(), old.chords().into_iter().collect());
                    Err("unavailable")
                } else {
                    registrations.borrow_mut().insert(chord);
                    Ok(())
                }
            },
            |chord| {
                registrations.borrow_mut().remove(&chord);
                Ok(())
            },
            || Ok(()),
        );
        assert!(result.is_err());
        assert_eq!(
            registrations.into_inner(),
            old.chords().into_iter().collect()
        );
    }

    #[test]
    fn persistence_failure_rolls_back_registrations() {
        let old = Routes::parse(&Settings::default()).unwrap();
        let mut settings = Settings::default();
        settings.shortcuts.dictate[0] = "Ctrl+Alt+V".into();
        let new = Routes::parse(&settings).unwrap();
        let registrations = RefCell::new(old.chords().into_iter().collect::<HashSet<_>>());
        let result = update_registrations(
            &old,
            &new,
            |chord| {
                registrations.borrow_mut().insert(chord);
                Ok::<_, &str>(())
            },
            |chord| {
                registrations.borrow_mut().remove(&chord);
                Ok::<_, &str>(())
            },
            || Err("write failed".into()),
        );
        assert_eq!(result.unwrap_err(), "write failed");
        assert_eq!(
            registrations.into_inner(),
            old.chords().into_iter().collect()
        );
    }
}
