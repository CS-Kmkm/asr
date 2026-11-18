#[cfg(test)]
use crate::types::ScopedStyleProfile;
use crate::types::{AppContext, Settings, StyleProfile};

pub(crate) const MAX_GUIDANCE_CHARS: usize = 300;
pub(crate) const MAX_SCOPE_CHARS: usize = 80;
pub(crate) const CATEGORIES: [&str; 7] = [
    "browser",
    "email",
    "messaging",
    "development",
    "document",
    "terminal",
    "other",
];

pub(crate) fn validate_profile(profile: &StyleProfile) -> Result<(), &'static str> {
    if !matches!(profile.formality.as_str(), "formal" | "casual")
        || !matches!(profile.detail.as_str(), "concise" | "detailed")
    {
        return Err("style profile values are invalid");
    }
    if let Some(guidance) = &profile.guidance {
        validate_text(guidance, MAX_GUIDANCE_CHARS)?;
    }
    Ok(())
}

pub(crate) fn validate_scope(scope: &str) -> Result<(), &'static str> {
    validate_text(scope, MAX_SCOPE_CHARS)?;
    if scope.trim() != scope || scope.matches(':').count() != 1 {
        return Err("invalid scope");
    }
    let Some((kind, value)) = scope.split_once(':') else {
        return Err("invalid scope");
    };
    if kind == "app" {
        if value.is_empty()
            || !value
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err("invalid app scope");
        }
    } else if kind == "category" {
        if !CATEGORIES.contains(&value) {
            return Err("unknown category scope");
        }
    } else {
        return Err("scope must be app:<key> or category:<category>");
    }
    Ok(())
}

pub(crate) fn validate_dictionary_scope(scope: Option<&str>) -> Result<(), &'static str> {
    match scope {
        None | Some("") | Some("global") => Ok(()),
        Some(value) => validate_scope(value),
    }
}

pub(crate) fn validate_settings_profiles(settings: &Settings) -> Result<(), &'static str> {
    if settings.learned_style_rules.len() > 100 {
        return Err("at most 100 learned style rules can be saved");
    }
    for rule in &settings.learned_style_rules {
        if !["formal", "casual", "bullets", "paragraphs"].contains(&rule.preference.as_str())
            || rule.modes.is_empty()
            || rule
                .modes
                .iter()
                .any(|mode| !crate::types::DICTIONARY_MODES.contains(&mode.as_str()))
        {
            return Err("learned style rule is invalid");
        }
        if rule.scope != "global" {
            validate_scope(&rule.scope)?;
        }
    }
    if settings.scoped_style_profiles.len() > 100 {
        return Err("at most 100 scoped style profiles can be saved");
    }
    if let Some(profile) = &settings.global_style_profile {
        validate_profile(profile)?;
    }
    let mut scopes = std::collections::HashSet::new();
    for scoped in &settings.scoped_style_profiles {
        validate_scope(&scoped.scope)?;
        validate_profile(&scoped.profile)?;
        if !scopes.insert(scoped.scope.as_str()) {
            return Err("scoped style profile scopes must be unique");
        }
    }
    Ok(())
}

fn validate_text(value: &str, max: usize) -> Result<(), &'static str> {
    if value.chars().count() > max || value.chars().any(char::is_control) {
        Err("value is too long or contains control characters")
    } else {
        Ok(())
    }
}

/// Fixed precedence: exact app > category > global. Without an app context
/// (lookup failure) only the global profile applies.
pub(crate) fn resolve_profile(
    settings: &Settings,
    context: Option<&AppContext>,
) -> Option<StyleProfile> {
    resolve_profile_for(settings, context, crate::types::DictionaryMode::Dictate)
}

pub(crate) fn resolve_profile_for(
    settings: &Settings,
    context: Option<&AppContext>,
    mode: crate::types::DictionaryMode,
) -> Option<StyleProfile> {
    if !settings.personalization_enabled {
        return None;
    }
    let scoped = context.and_then(|context| {
        let app_scope = context.app_key.as_deref().map(|key| format!("app:{key}"));
        let category_scope = format!("category:{}", context.category);
        settings
            .scoped_style_profiles
            .iter()
            .find(|item| app_scope.as_deref() == Some(item.scope.as_str()))
            .or_else(|| {
                settings
                    .scoped_style_profiles
                    .iter()
                    .find(|item| item.scope == category_scope)
            })
    });
    let base = scoped
        .map(|item| item.profile.clone())
        .or_else(|| settings.global_style_profile.clone());
    let base_rank = scoped.map_or(0, |item| if item.scope.starts_with("app:") { 2 } else { 1 });
    let mode = match mode {
        crate::types::DictionaryMode::Dictate => "dictate",
        crate::types::DictionaryMode::Edit => "edit",
        crate::types::DictionaryMode::Ask => "ask",
    };
    let mut rules: Vec<_> = settings
        .learned_style_rules
        .iter()
        .filter_map(|rule| {
            if !rule.modes.iter().any(|m| m == mode) {
                return None;
            }
            let rank = if rule.scope == "global" {
                0
            } else if context.is_some_and(|c| rule.scope == format!("category:{}", c.category)) {
                1
            } else if context
                .and_then(|c| c.app_key.as_ref())
                .is_some_and(|app| rule.scope == format!("app:{app}"))
            {
                2
            } else {
                return None;
            };
            // A broad learned preference must not override a more specific
            // manually configured profile. Confirmed rules at the same scope
            // may refine that profile.
            (rank >= base_rank).then_some((rank, rule))
        })
        .collect();
    if rules.is_empty() {
        return base;
    }
    rules.sort_by_key(|(rank, _)| *rank);
    let mut profile = base.unwrap_or(StyleProfile {
        formality: String::new(),
        detail: String::new(),
        guidance: None,
    });
    let mut layout = None;
    for (_, rule) in rules {
        match rule.preference.as_str() {
            "formal" | "casual" => profile.formality.clone_from(&rule.preference),
            "bullets" => layout = Some("Prefer bullet lists for enumerations; preserve every fact and do not turn ordinary prose into a list."),
            "paragraphs" => layout = Some("Prefer connected prose and paragraphs over bullet lists; preserve every fact."),
            _ => {},
        }
    }
    if let Some(layout) = layout {
        let guidance = profile.guidance.get_or_insert_with(String::new);
        if !guidance.is_empty() {
            guidance.push(' ');
        }
        guidance.push_str(layout);
    }
    Some(profile)
}

pub(crate) fn guidance(profile: &StyleProfile) -> String {
    let detail = match profile.detail.as_str() {
        "detailed" => "Prefer detailed phrasing of existing facts; never add new details.",
        "" => "Preserve the original level of detail.",
        _ => "Prefer concise phrasing.",
    };
    guidance_with_detail(profile, detail)
}

pub(crate) fn action_guidance(profile: &StyleProfile) -> String {
    let detail = if profile.detail.is_empty() {
        "Preserve the original level of detail."
    } else if profile.detail == "detailed" {
        "Prefer detailed responses when appropriate for the requested action."
    } else {
        "Prefer concise phrasing."
    };
    guidance_with_detail(profile, detail)
}

fn guidance_with_detail(profile: &StyleProfile, detail: &str) -> String {
    let formality = match profile.formality.as_str() {
        "formal" => "Use formal, professional wording.",
        "" => "Preserve the original register.",
        _ => "Use casual, conversational wording.",
    };
    let mut result = format!("{formality} {detail}");
    if let Some(extra) = profile
        .guidance
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        result.push(' ');
        result.push_str(extra.trim());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_scopes_are_valid_for_profiles_and_dictionary_entries() {
        assert!(validate_scope("category:terminal").is_ok());
        assert!(validate_dictionary_scope(Some("category:terminal")).is_ok());
    }

    fn profile(formality: &str, detail: &str) -> StyleProfile {
        StyleProfile {
            formality: formality.into(),
            detail: detail.into(),
            guidance: None,
        }
    }

    fn context(app_key: &str, category: &str) -> AppContext {
        AppContext {
            app_key: Some(app_key.into()),
            category: category.into(),
        }
    }

    #[test]
    fn profile_resolution_prefers_app_then_category_then_global() {
        let global = profile("formal", "concise");
        let category = profile("casual", "detailed");
        let app = profile("formal", "detailed");
        let mut settings = Settings {
            personalization_enabled: true,
            global_style_profile: Some(global.clone()),
            ..Settings::default()
        };
        // The category profile is listed first so list order cannot explain an
        // exact-app win.
        settings.scoped_style_profiles = vec![
            ScopedStyleProfile {
                scope: "category:development".into(),
                profile: category.clone(),
            },
            ScopedStyleProfile {
                scope: "app:code".into(),
                profile: app.clone(),
            },
        ];
        for _ in 0..2 {
            let exact_app =
                resolve_profile(&settings, Some(&context("code", "development"))).unwrap();
            assert_eq!(exact_app.formality, "formal");
            assert_eq!(exact_app, app);
            let category_only =
                resolve_profile(&settings, Some(&context("rider", "development"))).unwrap();
            assert_eq!(category_only.formality, "casual");
            assert_eq!(category_only, category);
            let no_match = resolve_profile(&settings, Some(&context("chrome", "browser"))).unwrap();
            assert_eq!(no_match, global);
            // Precedence is fixed; reordering the scoped list has no effect.
            settings.scoped_style_profiles.reverse();
        }
        settings.personalization_enabled = false;
        assert!(resolve_profile(&settings, Some(&context("code", "development"))).is_none());
    }

    #[test]
    fn profile_resolution_without_app_context_uses_global_only() {
        let global = profile("formal", "concise");
        let other = profile("casual", "detailed");
        let settings = Settings {
            personalization_enabled: true,
            global_style_profile: Some(global.clone()),
            scoped_style_profiles: vec![ScopedStyleProfile {
                scope: "category:other".into(),
                profile: other.clone(),
            }],
            ..Settings::default()
        };
        assert_eq!(resolve_profile(&settings, None), Some(global));
        // A successfully identified but unclassified app is still category:other.
        let unclassified = AppContext {
            app_key: Some("myeditor".into()),
            category: "other".into(),
        };
        assert_eq!(resolve_profile(&settings, Some(&unclassified)), Some(other));
        let no_global = Settings {
            global_style_profile: None,
            ..settings
        };
        assert_eq!(resolve_profile(&no_global, None), None);
    }

    #[test]
    fn learned_preferences_respect_manual_scope_precedence() {
        let mut settings = Settings {
            personalization_enabled: true,
            scoped_style_profiles: vec![ScopedStyleProfile {
                scope: "app:code".into(),
                profile: profile("formal", "concise"),
            }],
            learned_style_rules: vec![crate::types::LearnedStyleRule {
                preference: "casual".into(),
                scope: "global".into(),
                modes: vec!["dictate".into()],
            }],
            ..Settings::default()
        };
        let ctx = context("code", "development");
        assert_eq!(
            resolve_profile(&settings, Some(&ctx)).unwrap().formality,
            "formal"
        );
        settings.learned_style_rules[0].scope = "category:development".into();
        assert_eq!(
            resolve_profile(&settings, Some(&ctx)).unwrap().formality,
            "formal"
        );
        settings.learned_style_rules[0].scope = "app:code".into();
        assert_eq!(
            resolve_profile(&settings, Some(&ctx)).unwrap().formality,
            "casual"
        );
        settings.scoped_style_profiles[0].scope = "category:development".into();
        assert_eq!(
            resolve_profile(&settings, Some(&ctx)).unwrap().formality,
            "casual"
        );
    }

    #[test]
    fn profile_validation_rejects_controls_and_oversize_guidance() {
        let profile = StyleProfile {
            formality: "formal".into(),
            detail: "concise".into(),
            guidance: Some("\n".into()),
        };
        assert!(validate_profile(&profile).is_err());
        let profile = StyleProfile {
            formality: "formal".into(),
            detail: "concise".into(),
            guidance: Some("x".repeat(MAX_GUIDANCE_CHARS + 1)),
        };
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn dictionary_scope_validation_accepts_legacy_values_and_rejects_invalid_values() {
        for scope in [
            None,
            Some(""),
            Some("global"),
            Some("app:code"),
            Some("category:email"),
        ] {
            assert!(validate_dictionary_scope(scope).is_ok());
        }
        assert!(validate_dictionary_scope(Some("window:foo")).is_err());
        assert!(validate_dictionary_scope(Some("app:\ncode")).is_err());
        assert!(validate_dictionary_scope(Some(r"app:C:\Code")).is_err());
        assert!(validate_dictionary_scope(Some("category:unknown")).is_err());
    }

    #[test]
    fn settings_profile_validation_rejects_duplicate_scopes() {
        let settings = Settings {
            scoped_style_profiles: vec![
                ScopedStyleProfile {
                    scope: "app:code".into(),
                    profile: StyleProfile {
                        formality: "formal".into(),
                        detail: "concise".into(),
                        guidance: None,
                    },
                },
                ScopedStyleProfile {
                    scope: "app:code".into(),
                    profile: StyleProfile {
                        formality: "casual".into(),
                        detail: "detailed".into(),
                        guidance: None,
                    },
                },
            ],
            ..Settings::default()
        };
        assert!(validate_settings_profiles(&settings).is_err());
    }
}
