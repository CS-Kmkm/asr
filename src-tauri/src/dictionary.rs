//! Dictionary information shared by storage, prompt construction and validation.
/// Apply each character's case mapping independently so Greek final sigma
/// and length-changing mappings behave consistently in every caller.
pub(crate) fn fold(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CorrectionTerm {
    pub surface: String,
    pub readings: Vec<String>,
}

impl CorrectionTerm {
    pub(crate) fn prompt_hint(&self) -> String {
        if self.readings.is_empty() {
            self.surface.clone()
        } else {
            format!("{}<={}", self.surface, self.readings.join("|"))
        }
    }

    // Existing fact-check fixtures describe compact hints. Production never
    // decodes these delimiters: it receives the original structured fields.
    #[cfg(test)]
    pub(crate) fn from_fixture(hint: &str) -> Self {
        let (surface, readings) = hint.trim().split_once("<=").unwrap_or((hint.trim(), ""));
        Self {
            surface: surface.trim().into(),
            readings: readings
                .split('|')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
        }
    }
}
