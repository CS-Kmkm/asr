#![cfg(test)]

use super::*;

#[test]
fn a_failed_copy_never_claims_the_result_is_on_the_clipboard() {
    use injection::InjectionError;
    assert_eq!(ask_fallback_insertion(Ok(())), "clipboard_only");
    assert_eq!(
        ask_completion_message(ask_fallback_insertion(Ok(()))),
        "Ask result is on the clipboard."
    );
    let failed = Err(InjectionError::ClipboardUnavailable);
    let message = ask_completion_message(ask_fallback_insertion(failed));
    assert!(!message.contains("is on the clipboard"), "{message}");
    assert!(message.contains("available in this app"), "{message}");
    // A copy refused to protect the clipboard says so.
    let protected = Err(InjectionError::ClipboardProtected);
    let message = ask_completion_message(ask_fallback_insertion(protected));
    assert!(message.contains("left unchanged"), "{message}");
    assert!(message.contains("available in this app"), "{message}");
    assert_eq!(ask_completion_message("panel"), "Ask completed.");
    for insertion in ["selection", "caret"] {
        assert_eq!(ask_completion_message(insertion), "Ask completed.");
    }
}
