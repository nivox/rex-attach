//! Sanitize server-provided display text without forwarding terminal controls.
pub fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}
