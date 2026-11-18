//! Explicit writing review is isolated from conversion and never persists document text.
use anyhow::{Result, ensure};
use serde::Serialize;
#[derive(Serialize)]
pub struct Finding {
    pub rule: String,
    pub start_utf16: u32,
    pub end_utf16: u32,
    pub message: String,
    pub hint: Option<String>,
}
pub fn review(text: &str, whole_document: bool) -> Result<Vec<Finding>> {
    ensure!(
        text.len() <= 64 * 1024,
        "writing review is limited to 64 KiB"
    );
    let rules = noslop::rules::builtin_rules(Default::default())
        .into_iter()
        .filter(|rule| rule.meta().lane == noslop::diagnostic::Lane::Readability)
        .collect();
    let engine = noslop::engine::Engine::with_rules(rules, Default::default())?;
    let mut document = noslop::document::Document::plain_text(text);
    if !whole_document {
        document.kind = noslop::document::DocumentKind::Fragments;
    }
    let report = engine.lint(document);
    Ok(report
        .diagnostics
        .into_iter()
        .filter(|d| !d.is_suppressed())
        .filter_map(|d| {
            let prefix = text.get(..d.span.start)?;
            let through = text.get(..d.span.end)?;
            Some(Finding {
                rule: d.rule_id,
                start_utf16: kanade_ipc::utf16_len(prefix),
                end_utf16: kanade_ipc::utf16_len(through),
                message: d.message,
                hint: d.hint,
            })
        })
        .collect())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_offsets_are_utf16_and_do_not_include_document_text() {
        let text = "😀 この操作を行うことができます。";
        let findings = review(text, false).unwrap();
        assert!(!findings.is_empty());
        for finding in findings {
            assert!(finding.end_utf16 <= kanade_ipc::utf16_len(text));
            assert!(finding.start_utf16 <= finding.end_utf16);
        }
        assert!(review(&"あ".repeat(22_000), false).is_err());
    }
}
