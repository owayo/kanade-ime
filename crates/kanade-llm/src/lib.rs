// SPDX-License-Identifier: MIT
//! Optional local reranking of existing conversion candidates.
//!
//! The default build contains model management, qualification and a bounded
//! background service. Inference runs only in the separate `kanade-llm-worker`
//! process, built with the `llama` feature. Input text is never logged or stored.
//!
//! Every model-specific rule belongs to a registered [`profile::Profile`]:
//! vocabulary, tokenizer overrides, prompt format, start and end token,
//! corpora and gates. A model is imported, served, scored and qualified with
//! the one profile this build registers for it.
use kanade_ipc::{Privacy, Revision};

pub mod frame;
pub mod manager;
pub mod process;
pub mod profile;
pub mod prompt;
pub mod qualify;
pub mod service;
pub mod store;

pub use manager::Manager;
pub use service::{Poll, Rerank, Service};

/// Fixed bounds shared by the engine, the service and the worker.
pub mod limits {
    /// Left context sent to the worker, in characters. No profile uses more.
    pub const CONTEXT_CHARS: usize = 40;
    pub const READING_CHARS: usize = 64;
    pub const CANDIDATES: usize = 9;
    pub const CANDIDATE_CHARS: usize = 32;
    /// Unfocused segments before or after the focused one, in characters.
    pub const SURROUND_CHARS: usize = 64;
    /// Upper bound of any text field on the worker pipe.
    pub const TEXT_BYTES: usize = 256;
    pub const PROMPT_TOKENS: usize = 192;
    pub const CONTINUATION_TOKENS: usize = 112;
    pub const CONTEXT_TOKENS: u32 = 320;
    pub const THREADS: u8 = 4;
    /// Inference budget enforced inside the worker.
    pub const BUDGET_MS: u32 = 500;
    /// The parent kills a worker that has not answered by this time.
    pub const HARD_TIMEOUT_MS: u64 = 900;
    /// A result older than this is never applied to a displayed candidate list.
    pub const RESULT_TTL_MS: u64 = 700;
    pub const QUEUE: usize = 2;
    pub const CACHE: usize = 32;
}

/// One reranking request. All fields are bounded and free of control
/// characters. Markers, space handling and reading script are applied by the
/// active profile in the worker; a job carries plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// Trailing document context. A composition prefix also supplies context.
    pub context: String,
    /// Katakana reading of the whole composition.
    pub reading: String,
    /// Displayed surface before and after the focused segment.
    pub prefix: String,
    pub suffix: String,
    /// Alternative surfaces of the focused segment, in display order.
    pub candidates: Vec<String>,
}
impl Job {
    pub fn valid(&self) -> bool {
        let chars = |s: &str| s.chars().count();
        let clean = |s: &str| !s.chars().any(char::is_control);
        (!self.context.is_empty() || !self.prefix.trim().is_empty())
            && chars(&self.context) <= limits::CONTEXT_CHARS
            && clean(&self.context)
            && (2..=limits::READING_CHARS).contains(&chars(&self.reading))
            && self
                .reading
                .chars()
                .all(|c| matches!(c, 'ァ'..='ヺ' | 'ー'))
            && chars(&self.prefix) <= limits::SURROUND_CHARS
            && chars(&self.suffix) <= limits::SURROUND_CHARS
            && clean(&self.prefix)
            && clean(&self.suffix)
            && (2..=limits::CANDIDATES).contains(&self.candidates.len())
            && self.candidates.iter().enumerate().all(|(index, value)| {
                (1..=limits::CANDIDATE_CHARS).contains(&chars(value))
                    && clean(value)
                    && !self.candidates[..index].contains(value)
            })
            && [&self.context, &self.reading, &self.prefix, &self.suffix]
                .into_iter()
                .chain(&self.candidates)
                .all(|s| s.len() <= limits::TEXT_BYTES)
    }
}

/// The engine's default location: next to its own executable.
pub fn default_worker() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("KANADE_LLM_WORKER") {
        return path.into();
    }
    let name = if cfg!(windows) {
        "kanade-llm-worker.exe"
    } else {
        "kanade-llm-worker"
    };
    std::env::current_exe()
        .map(|exe| exe.with_file_name(name))
        .unwrap_or_else(|_| name.into())
}

/// A reading is sent only from a normal context, before any manual selection.
pub fn eligible(privacy: Privacy, reading: &str, manually_selected: bool) -> bool {
    privacy == Privacy::Normal
        && !manually_selected
        && (2..=limits::READING_CHARS).contains(&reading.chars().count())
        && reading
            .chars()
            .all(|c| matches!(c, 'ぁ'..='ゖ'|'ァ'..='ヺ'|'ー'))
}
pub fn current_result(request: Revision, current: Revision, manually_selected: bool) -> bool {
    request == current && !manually_selected
}
/// Rejects text that looks like an address, a path, a URL or a credential.
pub fn safe_context(text: &str) -> bool {
    text.len() <= 2048
        && !text.contains(['@', '/', '\\', ':'])
        && !text.split_whitespace().any(|s| {
            s.len() >= 32
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'='))
        })
}
/// Builds the bounded left context, or `None` when nothing may be sent.
///
/// Only the returned window leaves the engine thread, so the safety check
/// covers that window plus the token it may have been cut from.
pub fn context_window(preceding: &str) -> Option<String> {
    // Line breaks never reach a job. Everything else about the text, such as
    // the treatment of spaces, is the active profile's prompt format.
    let normalized: String = preceding
        .chars()
        .filter(|c| !matches!(c, '\n' | '\r'))
        .collect();
    let total = normalized.chars().count();
    // Examine a little more than is sent: a credential cut in half must not pass.
    let examined: String = normalized
        .chars()
        .skip(total.saturating_sub(limits::CONTEXT_CHARS * 2))
        .collect();
    if !safe_context(&examined) || examined.chars().any(char::is_control) {
        return None;
    }
    let window: String = normalized
        .chars()
        .skip(total.saturating_sub(limits::CONTEXT_CHARS))
        .collect();
    (!window.trim_matches([' ', '\u{3000}']).is_empty()).then_some(window)
}
pub fn katakana(reading: &str) -> String {
    reading
        .chars()
        .map(|c| match c {
            'ぁ'..='ゖ' => char::from_u32(c as u32 + 0x60).unwrap_or(c),
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_stale_and_sensitive_requests_are_rejected() {
        assert!(!eligible(Privacy::Private, "にほん", false));
        assert!(!eligible(Privacy::Unknown, "にほん", false));
        assert!(!eligible(Privacy::Password, "にほん", false));
        assert!(!eligible(Privacy::Normal, "にほん", true));
        assert!(!eligible(Privacy::Normal, "に", false));
        assert!(!eligible(Privacy::Normal, "にほん1", false));
        assert!(eligible(Privacy::Normal, "にほん", false));
        assert!(!safe_context("https://example.com/"));
        assert!(!safe_context("you@example.com"));
        assert!(!current_result(
            Revision {
                connection: 1,
                context: 1,
                composition: 1
            },
            Revision {
                connection: 1,
                context: 2,
                composition: 1
            },
            false
        ));
    }
    #[test]
    fn context_window_is_bounded_and_fails_closed() {
        assert_eq!(context_window(""), None);
        assert_eq!(context_window("  \n "), None);
        assert_eq!(context_window("\u{3000} \r\n"), None);
        assert_eq!(context_window("see https://example.com/ あ"), None);
        assert_eq!(context_window("you@example.com へ"), None);
        assert_eq!(context_window("あ\u{7}い"), None);
        let long = "あ".repeat(100) + "今日は\nいい 天気";
        let window = context_window(&long).unwrap();
        assert_eq!(window.chars().count(), limits::CONTEXT_CHARS);
        // Spaces are left to the profile; the model sees what it saw before.
        assert!(window.ends_with("今日はいい 天気"));
        assert!(
            profile::CATALOG[0]
                .profile
                .prompt
                .normalize(&window)
                .ends_with("今日はいい\u{3000}天気")
        );
        // A long credential-like token is refused even when only its tail would be sent.
        let token = "a".repeat(60);
        assert_eq!(context_window(&format!("鍵 {token}")), None);
    }
    #[test]
    fn job_bounds() {
        let job = Job {
            context: "今日は".into(),
            reading: katakana("こうえん"),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec!["公園".into(), "講演".into()],
        };
        assert!(job.valid());
        assert_eq!(job.reading, "コウエン");
        for edit in [
            |j: &mut Job| j.context.clear(),
            |j: &mut Job| j.context = "あ".repeat(limits::CONTEXT_CHARS + 1),
            |j: &mut Job| j.reading = "こうえん".into(),
            |j: &mut Job| j.reading = "コ".into(),
            |j: &mut Job| j.candidates.truncate(1),
            |j: &mut Job| j.candidates.push("公園".into()),
            |j: &mut Job| j.candidates.push("\u{1b}".into()),
            |j: &mut Job| j.candidates = vec!["あ".into(); limits::CANDIDATES + 1],
            |j: &mut Job| j.prefix = "あ".repeat(limits::SURROUND_CHARS + 1),
            |j: &mut Job| j.candidates.push("あ".repeat(limits::CANDIDATE_CHARS + 1)),
        ] {
            let mut bad = job.clone();
            edit(&mut bad);
            assert!(!bad.valid());
        }
    }
}
