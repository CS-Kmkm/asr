use crate::{parse_shortcut, types::Settings};
use tauri_plugin_global_shortcut::Shortcut;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Dictate,
    Translate,
    Ask,
    Edit,
    SelectedText,
}

#[derive(Clone)]
pub(crate) struct Route {
    pub(crate) chord: Shortcut,
    pub(crate) text: String,
    pub(crate) action: Action,
}

#[derive(Clone)]
pub(crate) struct Routes(pub(crate) Vec<Route>);

impl Routes {
    pub(crate) fn parse(settings: &Settings) -> Result<Self, String> {
        Self::parse_with_collisions(settings, false)
    }

    /// Existing settings may predate a mode and therefore reuse its default
    /// chord. Keep the first route active until the user resolves the overlap.
    pub(crate) fn parse_saved(settings: &Settings) -> Result<Self, String> {
        Self::parse_with_collisions(settings, true)
    }

    fn parse_with_collisions(settings: &Settings, allow_collisions: bool) -> Result<Self, String> {
        let mut routes = Vec::new();
        for (action, values) in [
            (Action::Dictate, &settings.shortcuts.dictate),
            (Action::Translate, &settings.shortcuts.translate),
            (Action::Ask, &settings.shortcuts.ask),
            (Action::Edit, &settings.shortcuts.edit),
        ] {
            if !(1..=4).contains(&values.len()) {
                return Err("each voice mode requires one to four shortcuts".into());
            }
            for text in values {
                Self::push(&mut routes, action, text, allow_collisions)?;
            }
        }
        Self::push(
            &mut routes,
            Action::SelectedText,
            &settings.translation_hotkey,
            allow_collisions,
        )?;
        Ok(Self(routes))
    }

    fn push(
        routes: &mut Vec<Route>,
        action: Action,
        text: &str,
        allow_collisions: bool,
    ) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("shortcut cannot be empty".into());
        }
        let chord = parse_shortcut(text)?;
        if routes.iter().any(|route: &Route| route.chord == chord) {
            return if allow_collisions {
                Ok(())
            } else {
                Err("shortcuts must be unique across all actions".into())
            };
        }
        routes.push(Route {
            chord,
            text: text.to_owned(),
            action,
        });
        Ok(())
    }

    pub(crate) fn find(&self, chord: Shortcut) -> Option<&Route> {
        self.0.iter().find(|route| route.chord == chord)
    }

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
        Routes::parse(candidate)
    }
}

/// Registration changes are applied before persistence; routing is published by
/// the caller only after this returns. A failed rollback requires a restart.
pub(crate) fn update_registrations<E: std::fmt::Display>(
    old: &Routes,
    new: &Routes,
    mut register: impl FnMut(Shortcut) -> Result<(), E>,
    mut unregister: impl FnMut(Shortcut) -> Result<(), E>,
    mut persist: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let old_chords = old.chords();
    let new_chords = new.chords();
    let removed: Vec<_> = old_chords
        .iter()
        .copied()
        .filter(|v| !new_chords.contains(v))
        .collect();
    let added: Vec<_> = new_chords
        .iter()
        .copied()
        .filter(|v| !old_chords.contains(v))
        .collect();
    let mut removed_done = Vec::new();
    let mut added_done = Vec::new();
    let mut failure = None;
    for chord in &added {
        match register(*chord) {
            Ok(()) => added_done.push(*chord),
            Err(error) => {
                failure = Some(format!("shortcut registration failed: {error}"));
                break;
            }
        }
    }
    if failure.is_none() {
        for chord in &removed {
            match unregister(*chord) {
                Ok(()) => removed_done.push(*chord),
                Err(error) => {
                    failure = Some(format!("shortcut removal failed: {error}"));
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
    let mut restoration_errors = Vec::new();
    for chord in removed_done {
        if let Err(error) = register(chord) {
            restoration_errors.push(error.to_string());
        }
    }
    for chord in added_done.into_iter().rev() {
        if let Err(error) = unregister(chord) {
            restoration_errors.push(error.to_string());
        }
    }
    if restoration_errors.is_empty() {
        Err(failure)
    } else {
        Err(format!(
            "{failure}; shortcut restoration failed, restart required: {}",
            restoration_errors.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::HashSet};

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
