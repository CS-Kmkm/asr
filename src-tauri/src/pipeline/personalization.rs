//! Resolve optional prompt hints off the runtime without adding app identity to prompts.
use crate::{
    correction_prompt::PromptPersonalization,
    dictionary::CorrectionTerm,
    personalization,
    storage::Storage,
    types::{AppContext, Settings},
};

#[derive(Default)]
pub(crate) struct ResolvedPersonalization {
    hints: Vec<CorrectionTerm>,
    guidance: Option<String>,
}

impl ResolvedPersonalization {
    pub(crate) fn prompt(&self) -> PromptPersonalization<'_> {
        PromptPersonalization {
            dictionary_hints: &self.hints,
            style_guidance: self.guidance.as_deref(),
        }
    }
    pub(crate) fn instruction(&self, base: &str) -> String {
        self.prompt().apply(base)
    }
}

/// Ask planning passes `None` for source: selected content must not influence
/// its dictionary hints or reveal itself through the planning instruction.
pub(crate) async fn resolve(
    storage: &Storage,
    settings: &Settings,
    context: Option<&AppContext>,
    spoken: &str,
    source: Option<&str>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Option<ResolvedPersonalization> {
    if *cancel.borrow() {
        return None;
    }
    let storage = storage.clone();
    let settings = settings.clone();
    let context = context.cloned();
    let mut text = spoken.to_owned();
    if let Some(source) = source {
        text.push('\n');
        text.push_str(source);
    }
    let mut work = tokio::task::spawn_blocking(move || {
        let guidance = personalization::resolve_profile(&settings, context.as_ref())
            .map(|profile| personalization::action_guidance(&profile));
        let hints = storage
            .dictionary_correction_terms(&text, context.as_ref())
            .unwrap_or_default();
        ResolvedPersonalization { hints, guidance }
    });
    let result = tokio::select! {
        biased;
        _ = async {
            loop {
                if *cancel.borrow() || cancel.changed().await.is_err() { return; }
            }
        } => {
            // A started blocking read may finish later; it owns no pipeline
            // state, provider request or write side effect.
            work.abort(); return None;
        }
        result = &mut work => result.unwrap_or_default(),
    };
    (!*cancel.borrow()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{NewDictionaryEntry, ScopedStyleProfile, StyleProfile};

    #[tokio::test]
    async fn captured_context_resolves_scoped_preferences_and_keeps_selected_source_out_of_planning(
    ) {
        let storage = Storage::in_memory().unwrap();
        for (reading, surface, scope) in [
            ("global-reading", "GlobalTerm", None),
            ("app-reading", "AppTerm", Some("app:code")),
            (
                "source-reading",
                "SourceOnlyTerm",
                Some("category:development"),
            ),
            ("hidden-reading", "HiddenTerm", Some("category:email")),
        ] {
            storage
                .add_dictionary_entry(&NewDictionaryEntry {
                    reading,
                    surface,
                    category: None,
                    aliases: &[],
                    priority: 0,
                    app_scope: scope,
                })
                .unwrap();
        }
        let settings = Settings {
            personalization_enabled: true,
            global_style_profile: Some(StyleProfile {
                formality: "formal".into(),
                detail: "concise".into(),
                guidance: Some("global style".into()),
            }),
            scoped_style_profiles: vec![ScopedStyleProfile {
                scope: "app:code".into(),
                profile: StyleProfile {
                    formality: "casual".into(),
                    detail: "concise".into(),
                    guidance: Some("app style".into()),
                },
            }],
            ..Settings::default()
        };
        let context = AppContext {
            app_key: Some("code".into()),
            category: "development".into(),
        };
        let spoken = "global-reading app-reading hidden-reading";
        let (_cancel_tx, cancel) = tokio::sync::watch::channel(false);
        let planning = resolve(
            &storage,
            &settings,
            Some(&context),
            spoken,
            None,
            cancel.clone(),
        )
        .await
        .unwrap();
        let prompt = planning.instruction("planning schema");
        assert!(
            prompt.contains("app style")
                && prompt.contains("GlobalTerm")
                && prompt.contains("AppTerm")
        );
        assert!(
            !prompt.contains("HiddenTerm")
                && !prompt.contains("SourceOnlyTerm")
                && !prompt.contains("source-reading")
        );
        let generation = resolve(
            &storage,
            &settings,
            Some(&context),
            spoken,
            Some("source-reading"),
            cancel.clone(),
        )
        .await
        .unwrap();
        assert!(generation
            .instruction("generation policy")
            .contains("SourceOnlyTerm"));
        let off = resolve(
            &storage,
            &Settings {
                personalization_enabled: false,
                ..settings
            },
            Some(&context),
            spoken,
            None,
            cancel,
        )
        .await
        .unwrap();
        assert!(off.guidance.is_none());
        assert!(!off.hints.is_empty());
    }
}
