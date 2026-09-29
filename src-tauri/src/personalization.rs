use crate::types::{AppContext, ScopedStyleProfile, Settings, StyleProfile};

pub(crate) const MAX_GUIDANCE_CHARS: usize = 300;
pub(crate) const MAX_SCOPE_CHARS: usize = 80;
pub(crate) const CATEGORIES: [&str; 6] = [
    "browser",
    "email",
    "messaging",
    "development",
    "document",
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

pub(crate) fn resolve_profile(settings: &Settings, context: &AppContext) -> Option<StyleProfile> {
    if !settings.personalization_enabled {
        return None;
    }
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
        .map(|item| item.profile.clone())
        .or_else(|| settings.global_style_profile.clone())
}

pub(crate) fn guidance(profile: &StyleProfile) -> String {
    let formality = match profile.formality.as_str() {
        "formal" => "Use formal, professional wording.",
        _ => "Use casual, conversational wording.",
    };
    let detail = match profile.detail.as_str() {
        "detailed" => "Prefer detailed phrasing of existing facts; never add new details.",
        _ => "Prefer concise phrasing.",
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
            let exact_app = resolve_profile(&settings, &context("code", "development")).unwrap();
            assert_eq!(exact_app.formality, "formal");
            assert_eq!(exact_app, app);
            let category_only =
                resolve_profile(&settings, &context("rider", "development")).unwrap();
            assert_eq!(category_only.formality, "casual");
            assert_eq!(category_only, category);
            let no_match = resolve_profile(&settings, &context("chrome", "browser")).unwrap();
            assert_eq!(no_match, global);
            // Precedence is fixed; reordering the scoped list has no effect.
            settings.scoped_style_profiles.reverse();
        }
        settings.personalization_enabled = false;
        assert!(resolve_profile(&settings, &context("code", "development")).is_none());
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
        let mut settings = Settings::default();
        settings.scoped_style_profiles = vec![
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
        ];
        assert!(validate_settings_profiles(&settings).is_err());
    }
}
