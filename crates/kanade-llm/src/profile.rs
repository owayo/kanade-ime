// SPDX-License-Identifier: MIT
//! The trusted registry: which model files may be imported, and the profile
//! that fixes how each of them is loaded, prompted, scored and qualified.
//!
//! Everything here is compiled in. No profile, prompt or corpus is ever read
//! from a file, a request or the environment. `locks/llm.lock.toml` records
//! the same values with their sources; a test keeps both in step.
use crate::prompt::{Prompt, Script, Space};
use sha2::{Digest, Sha256};

/// Runtime crate and the llama.cpp commit it vendors, unmodified.
pub const RUNTIME_CRATE: &str = "llama-cpp-sys-2";
pub const RUNTIME_VERSION: &str = "0.1.160";
pub const LLAMA_CPP_COMMIT: &str = "d81235049384534c167caea52b85a694f6103d14";
/// Reported by the worker and stored in every qualification. A worker that
/// also contains the test-only profiles says so and is refused by an ordinary
/// build.
pub const RUNTIME: &str = if cfg!(feature = "fixture-profiles") {
    "llama-cpp-sys-2 0.1.160 llama.cpp d8123504 cpu fixtures"
} else {
    "llama-cpp-sys-2 0.1.160 llama.cpp d8123504 cpu"
};

/// One importable model file.
#[derive(Debug)]
pub struct Model {
    pub id: &'static str,
    pub name: &'static str,
    pub file_name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
    pub source_url: &'static str,
    pub revision: &'static str,
    pub attribution: &'static str,
    /// The only profile this file may be used with.
    pub profile: &'static Profile,
}

/// Load-time tokenizer settings of a profile.
#[derive(Debug, PartialEq, Eq)]
pub struct Tokenizer {
    /// String metadata overrides applied when the file is loaded. Only
    /// `tokenizer.` keys are accepted.
    pub overrides: &'static [(&'static str, &'static str)],
    /// Token ID with which the independent reference marks text it cannot
    /// represent. The runtime must refuse exactly the parity texts that
    /// contain it.
    pub unknown: Option<u32>,
}
impl Tokenizer {
    pub fn sha256(&self) -> String {
        let mut spec = String::from("kanade-llm-tokenizer v1\n");
        for (key, value) in self.overrides {
            spec.push_str(&format!("override {key}={value}\n"));
        }
        spec.push_str(&format!("unknown={}\n", token(self.unknown)));
        hex(&Sha256::digest(spec.as_bytes()))
    }
}

/// Thresholds a measurement of this profile must meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gates {
    pub min_ranking_per_mille: u32,
    pub max_p95_ms: u32,
}

/// What is done with a user's text. Selects the consent wording, so a profile
/// with another contract cannot reuse a consent given for this one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Processing {
    /// Bounded left context, reading and displayed candidates are scored in a
    /// separate local process. Nothing is sent, logged or stored.
    LocalRerank,
}

/// How one model family is loaded, prompted, scored and qualified.
#[derive(Debug)]
pub struct Profile {
    pub id: &'static str,
    pub version: u32,
    /// Digest of the vocabulary as the pinned runtime exposes it.
    pub vocab_sha256: &'static str,
    pub vocab_size: u32,
    pub tokenizer: Tokenizer,
    pub prompt: Prompt,
    /// Token placed before the conditioning text, if the model expects one.
    pub start: Option<u32>,
    /// Token that ends a conversion in the model's training format.
    pub end: u32,
    /// Expected token IDs from an independent tokenizer, as JSON.
    pub parity: &'static str,
    /// Context-dependent choices, one JSON object per line.
    pub ranking: &'static str,
    /// Text of several tokens per character, repeated by the bounds check to
    /// exceed the token limit.
    pub oversize: &'static str,
    pub gates: Gates,
    pub processing: Processing,
}
impl Profile {
    pub fn parity_sha256(&self) -> String {
        hex(&Sha256::digest(self.parity.as_bytes()))
    }
    pub fn ranking_sha256(&self) -> String {
        hex(&Sha256::digest(self.ranking.as_bytes()))
    }
    /// Identity of the whole profile. A qualification is bound to it; the
    /// worker reports the one it applied.
    pub fn sha256(&self) -> String {
        hex(&self.digest())
    }
    pub fn digest(&self) -> [u8; 32] {
        let code = |text: &str| {
            text.chars()
                .map(|c| format!("U+{:04X}", c as u32))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let spec = format!(
            "kanade-llm-profile v1\nid={} version={}\nvocab={} size={}\ntokenizer={}\nprompt={}\n\
             start={} end={}\nparity={}\nranking={}\noversize={}\n\
             gates=min_ranking_per_mille:{},max_p95_ms:{}\nprocessing={:?}\n",
            self.id,
            self.version,
            self.vocab_sha256,
            self.vocab_size,
            self.tokenizer.sha256(),
            self.prompt.sha256(),
            token(self.start),
            self.end,
            self.parity_sha256(),
            self.ranking_sha256(),
            code(self.oversize),
            self.gates.min_ranking_per_mille,
            self.gates.max_p95_ms,
            self.processing,
        );
        Sha256::digest(spec.as_bytes()).into()
    }
    /// Structural checks of a registration. Every registered profile passes;
    /// the worker refuses to load with one that does not.
    pub fn well_formed(&self) -> bool {
        let name = |id: &str| {
            (1..=64).contains(&id.len())
                && id.bytes().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'-')
                })
        };
        name(self.id)
            && self.version > 0
            && is_sha256(self.vocab_sha256)
            && self.vocab_size > 0
            && self.end < self.vocab_size
            && self.start.is_none_or(|start| start < self.vocab_size)
            && self
                .tokenizer
                .unknown
                .is_none_or(|unknown| unknown < self.vocab_size)
            && self.tokenizer.overrides.len() <= MAX_OVERRIDES
            && self.tokenizer.overrides.iter().all(|(key, value)| {
                let text = |s: &str| {
                    (1..OVERRIDE_BYTES).contains(&s.len())
                        && s.bytes().all(|c| c.is_ascii_graphic() && c != b'=')
                };
                key.starts_with("tokenizer.") && text(key) && text(value)
            })
            && self.prompt.well_formed()
            && !self.oversize.is_empty()
            && !self.prompt.reserves(self.oversize)
            && !self.oversize.chars().any(char::is_control)
            && self.gates.min_ranking_per_mille <= 1000
            && self.gates.max_p95_ms > 0
            && (self.gates.max_p95_ms as u64) < crate::limits::RESULT_TTL_MS
    }
}
/// Bounds of the runtime's override record: a short list of short strings.
pub const MAX_OVERRIDES: usize = 4;
pub const OVERRIDE_BYTES: usize = 128;

fn token(id: Option<u32>) -> String {
    id.map_or("none".into(), |id| id.to_string())
}

/// The sole model accepted by ordinary builds. Token IDs and weights are pinned together.
// Keep persistent profile and model IDs stable for installed qualifications and consents.
pub static TSUMUGI: Profile = Profile {
    id: "tsumugi-s",
    version: 1,
    vocab_sha256: "9026a21e3c4c916f5cfe37d0c081e32b08eafad1cdb08d616f81cfacd0f8c7f0",
    vocab_size: 7680,
    tokenizer: Tokenizer {
        overrides: &[],
        unknown: None,
    },
    prompt: Prompt {
        version: 1,
        context: "\u{E080}",
        input: "\u{E081}",
        output: "\u{E082}",
        space: Space::Ideographic,
        reading: Script::Katakana,
        reserved: &[('\u{E080}', '\u{E08F}')],
    },
    start: None,
    end: 259,
    parity: include_str!("../data/tokenizer-parity.json"),
    ranking: include_str!("../data/ranking.jsonl"),
    oversize: "\u{20BB7}",
    gates: Gates {
        min_ranking_per_mille: 900,
        max_p95_ms: 300,
    },
    processing: Processing::LocalRerank,
};

/// The fixed model used by the engine and isolated worker.
pub static CATALOG: &[Model] = &[Model {
    id: "tsumugi-s-f16",
    name: "Tsumugi 26.11.102",
    file_name: "tsumugi.gguf",
    size: 66507360,
    sha256: "62568f68fb2a549f0fa61ff62db38892ac7245d1bce0fa9ea8928ae70bbe3af7",
    license: "LicenseRef-Kanade-Model-1.0",
    license_url: "https://github.com/owayo/kanade-ime/blob/main/docs/llm.md#tsumugi-model",
    source_url: "https://github.com/owayo/kanade-ime/blob/main/docs/llm.md",
    revision: "a7196909485ef2978ad7c9f0275d8f0be3f4782692428a27fe58e480c56e32cd",
    attribution: "Kanade",
    profile: &TSUMUGI,
}];

/// Every model this build accepts: the catalogue, plus the fixtures in a test
/// build.
pub fn models() -> impl Iterator<Item = &'static Model> {
    #[cfg(any(test, feature = "fixture-profiles"))]
    let fixtures = fixture::MODELS.iter();
    #[cfg(not(any(test, feature = "fixture-profiles")))]
    let fixtures: std::slice::Iter<'static, Model> = [].iter();
    CATALOG.iter().chain(fixtures)
}
/// The registered model with this ID. An unknown ID has no profile.
pub fn model(id: &str) -> Option<&'static Model> {
    models().find(|model| model.id == id)
}

/// What the user accepts on import. The digest binds a stored consent to this
/// exact wording, licence and file.
pub fn terms(model: &Model) -> String {
    match model.profile.processing {
        Processing::LocalRerank => format!(
            "kanade-llm-terms v1\n\
             モデル: {} (SHA-256 {})\n\
             ライセンス: {} {}\n\
             作者: {} / 入手元: {}\n\
             Windows版にはモデルファイルが付属します。利用者の同意後にモデルを取り込みます。\n\
             ファイルの利用と再配布には上記のライセンスが適用されます。\n\
             有効にすると、カーソル直前の文字列 (最大 {} 文字)、読み、表示中の候補を、\
             この端末内の別プロセスで処理します。\n\
             ネットワークへの送信、ログへの記録、保存は行いません。\n",
            model.id,
            model.sha256,
            model.license,
            model.license_url,
            model.attribution,
            model.source_url,
            crate::limits::CONTEXT_CHARS,
        ),
    }
}
pub fn terms_sha256(model: &Model) -> String {
    hex(&Sha256::digest(terms(model).as_bytes()))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Profiles that exist only to test that nothing is shared between profiles.
/// They are absent from an ordinary build and never part of [`CATALOG`].
#[cfg(any(test, feature = "fixture-profiles"))]
pub mod fixture {
    use super::*;

    /// Expected IDs copied from the independent reference of the vocabulary
    /// that `ALT` shares with the catalogue model.
    const ALT_PARITY: &str = r#"{"cases":[{"text":"にほん","tokens":[920,936,960]},{"text":"日本","tokens":[3195,3318]},{"text":"全角　空白","tokens":[1381,6027,856,4799,4516]},{"text":"𠮷野家","tokens":[622,183,6636,2246]},{"text":"ABC 123","tokens":[65,66,67,32,49,50,51]}]}"#;
    const ALT_RANKING: &str = r#"{"context": "川に新しい", "reading": "はしがかけられた", "prefix": "", "suffix": "が架けられた", "candidates": ["橋", "箸", "端"], "expected": "橋"}
{"context": "子供たちは近くの", "reading": "こうえんであそんだ", "prefix": "", "suffix": "で遊んだ", "candidates": ["講演", "公園"], "expected": "公園"}
{"context": "今日は朝から", "reading": "あめがふっている", "prefix": "", "suffix": "が降っている", "candidates": ["飴", "雨"], "expected": "雨"}
"#;
    /// Another format for the catalogue model's file: other markers, no space
    /// mapping, hiragana readings, a start token, another end token, its own
    /// corpora and gates.
    pub static ALT_PROFILE: Profile = Profile {
        id: "fixture-alt",
        version: 3,
        vocab_sha256: TSUMUGI.vocab_sha256,
        vocab_size: TSUMUGI.vocab_size,
        tokenizer: Tokenizer {
            overrides: &[("tokenizer.ggml.pre", "gpt-2")],
            unknown: Some(0),
        },
        prompt: Prompt {
            version: 2,
            context: "『",
            input: "「",
            output: "』",
            space: Space::Keep,
            reading: Script::Hiragana,
            reserved: &[('「', '』')],
        },
        start: Some(2),
        end: 2,
        parity: ALT_PARITY,
        ranking: ALT_RANKING,
        oversize: "\u{20BB7}",
        // The model was not trained on this format; only stability, bounds
        // and tokenization are meaningful.
        gates: Gates {
            min_ranking_per_mille: 0,
            max_p95_ms: 400,
        },
        processing: Processing::LocalRerank,
    };
    /// A profile for a file that does not exist: multi-character markers and
    /// two overrides. Never loaded by a runtime.
    pub static OTHER_PROFILE: Profile = Profile {
        id: "fixture-other",
        version: 1,
        vocab_sha256: "6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b",
        vocab_size: 100,
        tokenizer: Tokenizer {
            overrides: &[
                ("tokenizer.ggml.pre", "default"),
                ("tokenizer.ggml.model", "gpt2"),
            ],
            unknown: None,
        },
        prompt: Prompt {
            version: 1,
            context: "<ctx>",
            input: "<in>",
            output: "<out>",
            space: Space::Keep,
            reading: Script::Katakana,
            reserved: &[],
        },
        start: None,
        end: 7,
        parity: r#"{"cases":[{"text":"a","tokens":[5]},{"text":"b","tokens":[6]}]}"#,
        ranking: "{\"context\": \"今日は\", \"reading\": \"こうえん\", \"prefix\": \"\", \"suffix\": \"\", \"candidates\": [\"公園\", \"講演\"], \"expected\": \"公園\"}\n",
        oversize: "x",
        gates: Gates {
            min_ranking_per_mille: 500,
            max_p95_ms: 50,
        },
        processing: Processing::LocalRerank,
    };
    pub static MODELS: &[Model] = &[
        // The catalogue model's file under another profile.
        Model {
            id: "fixture-alt-q5km",
            name: "fixture: alternative profile",
            profile: &ALT_PROFILE,
            ..copy(&CATALOG[0])
        },
        Model {
            id: "fixture-other-file",
            name: "fixture: another file",
            file_name: "other.gguf",
            size: 4096,
            sha256: "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a",
            profile: &OTHER_PROFILE,
            ..copy(&CATALOG[0])
        },
    ];
    const fn copy(model: &Model) -> Model {
        Model {
            id: model.id,
            name: model.name,
            file_name: model.file_name,
            size: model.size,
            sha256: model.sha256,
            license: model.license,
            license_url: model.license_url,
            source_url: model.source_url,
            revision: model.revision,
            attribution: model.attribution,
            profile: model.profile,
        }
    }
    pub fn alt() -> &'static Model {
        &MODELS[0]
    }
    pub fn other() -> &'static Model {
        &MODELS[1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lock() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../locks/llm.lock.toml"
        ))
        .unwrap()
    }
    /// Lines of one table, up to the next table header.
    fn section<'a>(lock: &'a str, header: &str) -> Vec<&'a str> {
        lock.lines()
            .skip_while(|line| *line != header)
            .skip(1)
            .take_while(|line| !line.starts_with('['))
            .collect()
    }
    #[track_caller]
    fn entry(lines: &[&str], key: &str, value: &str) {
        assert!(
            lines.iter().any(|line| {
                *line == format!("{key} = \"{value}\"") || *line == format!("{key} = {value}")
            }),
            "lock entry expected: {key} = {value}"
        );
    }
    #[test]
    fn the_lock_file_records_the_runtime_and_every_registered_model_and_profile() {
        let lock = lock();
        let runtime = section(&lock, "[runtime]");
        entry(&runtime, "crate", RUNTIME_CRATE);
        entry(&runtime, "version", RUNTIME_VERSION);
        entry(&runtime, "llama_cpp_commit", LLAMA_CPP_COMMIT);
        assert!(RUNTIME.contains(RUNTIME_CRATE) && RUNTIME.contains(RUNTIME_VERSION));
        assert!(RUNTIME.contains(&LLAMA_CPP_COMMIT[..8]));
        // Only a worker that contains the test-only profiles says so.
        assert_eq!(
            RUNTIME.ends_with(" fixtures"),
            cfg!(feature = "fixture-profiles")
        );

        let mut profiles: Vec<&Profile> = Vec::new();
        for model in CATALOG {
            let lines = section(&lock, &format!("[model.\"{}\"]", model.id));
            entry(&lines, "profile", model.profile.id);
            entry(&lines, "source", model.source_url);
            entry(&lines, "revision", model.revision);
            let version = model
                .name
                .strip_prefix("Tsumugi ")
                .expect("versioned model name");
            entry(&lines, "version", version);
            entry(
                &lines,
                "download_url",
                &format!(
                    "https://github.com/owayo/kanade-ime/releases/download/tsumugi-{version}/{}",
                    model.file_name
                ),
            );
            entry(&lines, "file", model.file_name);
            entry(&lines, "size", &model.size.to_string());
            entry(&lines, "sha256", model.sha256);
            entry(&lines, "license", model.license);
            entry(&lines, "attribution", model.attribution);
            entry(&lines, "terms_sha256", &terms_sha256(model));
            if !profiles.iter().any(|known| known.id == model.profile.id) {
                profiles.push(model.profile);
            }
        }
        for profile in &profiles {
            let lines = section(&lock, &format!("[profile.\"{}\"]", profile.id));
            let overrides: Vec<String> = profile
                .tokenizer
                .overrides
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            for (key, value) in [
                ("version", profile.version.to_string()),
                ("sha256", profile.sha256()),
                ("vocab_sha256", profile.vocab_sha256.into()),
                ("vocab_size", profile.vocab_size.to_string()),
                ("tokenizer_sha256", profile.tokenizer.sha256()),
                ("tokenizer_overrides", overrides.join(";")),
                ("unknown_token", token(profile.tokenizer.unknown)),
                ("prompt_version", profile.prompt.version.to_string()),
                ("prompt_sha256", profile.prompt.sha256()),
                ("start_token", token(profile.start)),
                ("end_token", profile.end.to_string()),
                ("tokenizer_parity_sha256", profile.parity_sha256()),
                ("ranking_sha256", profile.ranking_sha256()),
                (
                    "min_ranking_per_mille",
                    profile.gates.min_ranking_per_mille.to_string(),
                ),
                ("max_p95_ms", profile.gates.max_p95_ms.to_string()),
            ] {
                entry(&lines, key, &value);
            }
        }
        // Nothing is recorded that this build does not register.
        let tables = |prefix: &str| {
            lock.lines()
                .filter(|line| line.starts_with(prefix) && line.ends_with("\"]"))
                .count()
        };
        assert_eq!(tables("[model.\""), CATALOG.len());
        assert_eq!(tables("[profile.\""), profiles.len());

        // The resolved registry package is the pinned one.
        let cargo =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
                .unwrap();
        let package = cargo
            .split("[[package]]")
            .find(|package| package.contains(&format!("name = \"{RUNTIME_CRATE}\"")))
            .unwrap();
        assert!(package.contains(&format!("version = \"{RUNTIME_VERSION}\"")));
        let checksum = package
            .lines()
            .find_map(|line| line.strip_prefix("checksum = "))
            .unwrap()
            .trim_matches('"');
        entry(&runtime, "checksum", checksum);
    }
    #[test]
    fn the_catalogue_lists_only_the_existing_model_with_its_pinned_identity() {
        assert_eq!(CATALOG.len(), 1);
        let model = &CATALOG[0];
        assert_eq!(model.id, "tsumugi-s-f16");
        assert_eq!(model.size, 66507360);
        assert_eq!(
            model.sha256,
            "62568f68fb2a549f0fa61ff62db38892ac7245d1bce0fa9ea8928ae70bbe3af7"
        );
        assert_eq!(model.license, "LicenseRef-Kanade-Model-1.0");
        let profile = model.profile;
        assert_eq!((profile.start, profile.end), (None, 259));
        assert!(profile.tokenizer.overrides.is_empty());
        assert_eq!(profile.vocab_size, 7680);
        assert!(self::model("another-model").is_none());
        // Test-only profiles are reachable by ID in a test build, but the
        // catalogue never contains them.
        assert!(CATALOG.iter().all(|m| !m.id.starts_with("fixture")));
        assert!(std::ptr::eq(model, self::model(model.id).unwrap()));
        assert_eq!(
            self::model("fixture-alt-q5km").unwrap().profile.id,
            "fixture-alt"
        );
        for unknown in [
            "",
            "unknown-model",
            "unsupported-profile",
            "unregistered",
            "UNREGISTERED-F16",
        ] {
            assert!(self::model(unknown).is_none());
        }
    }
    #[test]
    fn registrations_are_well_formed_and_unambiguous() {
        let all: Vec<&Model> = models().collect();
        for (index, model) in all.iter().enumerate() {
            assert!(model.profile.well_formed(), "{}", model.id);
            assert!(is_sha256(model.sha256) && model.size > 0);
            assert!(is_sha256(&terms_sha256(model)));
            for other in &all[..index] {
                assert_ne!(model.id, other.id);
                // One file identity belongs to one catalogue entry.
                if CATALOG.iter().any(|m| m.id == model.id)
                    && CATALOG.iter().any(|m| m.id == other.id)
                {
                    assert_ne!(model.sha256, other.sha256);
                }
                // The same profile ID always means the same profile.
                if model.profile.id == other.profile.id {
                    assert!(std::ptr::eq(model.profile, other.profile));
                }
            }
        }
    }
    #[test]
    fn malformed_registrations_are_detected() {
        fn base() -> Profile {
            Profile {
                id: "p",
                version: 1,
                vocab_sha256: fixture::OTHER_PROFILE.vocab_sha256,
                vocab_size: 100,
                tokenizer: Tokenizer {
                    overrides: &[("tokenizer.ggml.pre", "gpt-2")],
                    unknown: Some(0),
                },
                prompt: Prompt {
                    version: 1,
                    context: "<c>",
                    input: "<i>",
                    output: "<o>",
                    space: Space::Keep,
                    reading: Script::Katakana,
                    reserved: &[],
                },
                start: Some(1),
                end: 2,
                parity: "",
                ranking: "",
                oversize: "x",
                gates: Gates {
                    min_ranking_per_mille: 900,
                    max_p95_ms: 300,
                },
                processing: Processing::LocalRerank,
            }
        }
        assert!(base().well_formed());
        let edits: [fn(&mut Profile); 15] = [
            |p| p.id = "Upper",
            |p| p.id = "",
            |p| p.version = 0,
            |p| p.vocab_sha256 = "abc",
            |p| p.end = 100,
            |p| p.start = Some(100),
            |p| p.tokenizer.unknown = Some(100),
            // Overrides may only touch tokenizer metadata, with bounded text.
            |p| p.tokenizer.overrides = &[("general.architecture", "x")],
            |p| p.tokenizer.overrides = &[("tokenizer.ggml.pre", "")],
            |p| p.tokenizer.overrides = &[("tokenizer.ggml.pre", "a b")],
            |p| p.tokenizer.overrides = &[("tokenizer.a", "1"); MAX_OVERRIDES + 1],
            |p| p.prompt.input = "<c>",
            |p| p.oversize = "<i>",
            |p| p.gates.min_ranking_per_mille = 1001,
            |p| p.gates.max_p95_ms = crate::limits::RESULT_TTL_MS as u32,
        ];
        for (index, edit) in edits.iter().enumerate() {
            let mut bad = base();
            edit(&mut bad);
            assert!(!bad.well_formed(), "edit {index}");
        }
        let long: &'static str =
            Box::leak(format!("tokenizer.{}", "k".repeat(OVERRIDE_BYTES)).into_boxed_str());
        let mut bad = base();
        bad.tokenizer.overrides = Box::leak(Box::new([(long, "v")]));
        assert!(!bad.well_formed());
    }
    #[test]
    fn profiles_share_no_prompt_tokenizer_end_token_corpus_or_gate() {
        let tsumugi = &TSUMUGI;
        let alt = &fixture::ALT_PROFILE;
        let other = &fixture::OTHER_PROFILE;
        // `alt` is bound to the same file and vocabulary, and still differs in
        // everything a profile owns.
        assert_eq!(fixture::alt().sha256, CATALOG[0].sha256);
        assert_eq!(alt.vocab_sha256, tsumugi.vocab_sha256);
        assert_ne!(alt.prompt.sha256(), tsumugi.prompt.sha256());
        assert_ne!((alt.start, alt.end), (tsumugi.start, tsumugi.end));
        assert_ne!(alt.parity_sha256(), tsumugi.parity_sha256());
        assert_ne!(alt.ranking_sha256(), tsumugi.ranking_sha256());
        assert_ne!(alt.gates, tsumugi.gates);
        assert_ne!(other.tokenizer.sha256(), tsumugi.tokenizer.sha256());
        let digests = [tsumugi.sha256(), alt.sha256(), other.sha256()];
        assert!(digests[0] != digests[1] && digests[1] != digests[2] && digests[0] != digests[2]);
    }
    #[test]
    fn every_owned_value_is_part_of_the_profile_digest() {
        fn copy(p: &Profile) -> Profile {
            Profile {
                id: p.id,
                version: p.version,
                vocab_sha256: p.vocab_sha256,
                vocab_size: p.vocab_size,
                tokenizer: Tokenizer {
                    overrides: p.tokenizer.overrides,
                    unknown: p.tokenizer.unknown,
                },
                prompt: Prompt {
                    version: p.prompt.version,
                    context: p.prompt.context,
                    input: p.prompt.input,
                    output: p.prompt.output,
                    space: p.prompt.space,
                    reading: p.prompt.reading,
                    reserved: p.prompt.reserved,
                },
                start: p.start,
                end: p.end,
                parity: p.parity,
                ranking: p.ranking,
                oversize: p.oversize,
                gates: p.gates,
                processing: p.processing,
            }
        }
        let base = &TSUMUGI;
        assert_eq!(copy(base).sha256(), base.sha256());
        let edits: [fn(&mut Profile); 16] = [
            |p| p.id = "tsumugi-v3.2-small-b",
            |p| p.version += 1,
            |p| p.vocab_sha256 = fixture::OTHER_PROFILE.vocab_sha256,
            |p| p.vocab_size += 1,
            |p| p.tokenizer.overrides = &[("tokenizer.ggml.pre", "default")],
            |p| p.tokenizer.overrides = &[("tokenizer.ggml.pre", "gpt-2")],
            |p| p.tokenizer.unknown = Some(0),
            |p| p.prompt.space = Space::Keep,
            |p| p.prompt.output = "\u{EE05}",
            |p| p.start = Some(2),
            |p| p.end = 2,
            |p| p.parity = fixture::ALT_PROFILE.parity,
            |p| p.ranking = fixture::ALT_PROFILE.ranking,
            |p| p.oversize = "x",
            |p| p.gates.min_ranking_per_mille -= 1,
            |p| p.gates.max_p95_ms += 1,
        ];
        let mut seen = vec![base.sha256()];
        for edit in edits {
            let mut changed = copy(base);
            edit(&mut changed);
            let digest = changed.sha256();
            assert!(!seen.contains(&digest));
            seen.push(digest);
        }
    }
    #[test]
    fn terms_name_the_licence_and_the_local_processing() {
        let model = &CATALOG[0];
        let terms = terms(model);
        for part in [
            model.license,
            model.license_url,
            model.sha256,
            model.attribution,
        ] {
            assert!(terms.contains(part));
        }
        assert!(terms.contains("利用者の同意後にモデルを取り込みます"));
        assert!(terms.contains("この端末内の別プロセスで処理します"));
        assert!(terms.contains("最大 40 文字"));
        assert_eq!(
            terms_sha256(model),
            "b4002897e8d631a4b5030d954b78834f1796167113ba911ec2d0db3d89300e58"
        );
        // Terms name the model, so no consent carries over to another one.
        assert_ne!(terms_sha256(fixture::alt()), terms_sha256(model));
    }
}
