//! Stable IPC codes and the shared English/Japanese application catalogue.
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(serde::Deserialize)]
pub(crate) struct Message {
    pub code: String,
    pub ja: String,
}

pub(crate) fn catalogue() -> &'static BTreeMap<String, Message> {
    static CATALOGUE: OnceLock<BTreeMap<String, Message>> = OnceLock::new();
    CATALOGUE.get_or_init(|| {
        let mut catalogue: BTreeMap<String, Message> =
            serde_json::from_str(include_str!("../../src/backend-messages.json"))
                .expect("the bundled backend message catalogue must be valid");
        let causes = catalogue
            .iter()
            .filter(|(_, message)| message.code.starts_with("ai_") && !message.code.contains("__"))
            .map(|(en, message)| (en.clone(), message.code.clone(), message.ja.clone()))
            .collect::<Vec<_>>();
        let outcomes = catalogue
            .iter()
            .filter_map(|(en, message)| {
                message
                    .code
                    .strip_prefix("outcome_")
                    .map(|suffix| (en.clone(), suffix.to_owned(), message.ja.clone()))
            })
            .collect::<Vec<_>>();
        for (en, code, ja) in causes {
            for (ending, suffix, translated_ending) in &outcomes {
                catalogue.insert(
                    format!("{en} {ending}"),
                    Message {
                        code: format!("{code}__{suffix}"),
                        ja: format!("{ja} {translated_ending}"),
                    },
                );
            }
        }
        assert!(catalogue
            .values()
            .all(|message| !message.code.is_empty() && !message.ja.is_empty()));
        catalogue
    })
}

pub(crate) fn code(message: &str) -> Option<&'static str> {
    catalogue()
        .get(message)
        .or_else(|| {
            catalogue()
                .iter()
                .find(|(template, _)| matches_template(template, message))
                .map(|(_, entry)| entry)
        })
        .map(|entry| entry.code.as_str())
}

fn matches_template(template: &str, message: &str) -> bool {
    let Some(start) = template.find('{') else {
        return false;
    };
    let Some(end) = template[start..].find('}').map(|end| start + end + 1) else {
        return false;
    };
    let (prefix, suffix) = (&template[..start], &template[end..]);
    !suffix.contains('{')
        && prefix.len() + suffix.len() >= 8
        && message.len() >= prefix.len() + suffix.len()
        && message.starts_with(prefix)
        && message.ends_with(suffix)
}

pub(crate) fn english(code: &str) -> &'static str {
    catalogue()
        .iter()
        .find(|(_, message)| message.code == code)
        .or_else(|| {
            catalogue()
                .iter()
                .find(|(_, message)| message.code == "ai_provider_error")
        })
        .map_or("The AI provider returned an error.", |(message, _)| {
            message.as_str()
        })
}

#[cfg(test)]
mod source_check;
#[cfg(test)]
mod tests;
