// SPDX-License-Identifier: MIT
//! Private, bounded frames between the engine-side service and the inference worker.
//!
//! Frames carry input text only through anonymous pipes. Nothing here is logged or stored.
use crate::{Job, limits};
use std::io::{self, Read, Write};

/// Version 2 added the model ID to `Load` and the applied profile to `Loaded`.
pub const PROTOCOL: u32 = 2;
/// Largest accepted frame body. A scoring request is far smaller.
pub const MAX_FRAME: usize = 64 * 1024;
const MAX_PATH: usize = 4096;
const MAX_MODEL_ID: usize = 64;
const MAX_TOKENIZE_TEXTS: usize = 256;
const MAX_TOKENIZE_CHARS: usize = 128;
const MAX_TOKEN_IDS: usize = 512;

/// Fixed failure reasons. They never include input-dependent text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Fault {
    Malformed = 1,
    NotLoaded = 2,
    ModelUnreadable = 3,
    ModelRejected = 4,
    TooLong = 5,
    Deadline = 6,
    Decode = 7,
    Tokenizer = 8,
    Internal = 9,
    AlreadyLoaded = 10,
    ModelChanged = 11,
    /// The model ID is not registered in the worker's build.
    UnknownModel = 12,
    /// The file or its vocabulary is not the one registered for the model.
    ProfileMismatch = 13,
    /// Request text contains a marker or reserved character of the profile.
    Reserved = 14,
}
impl Fault {
    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Malformed,
            2 => Self::NotLoaded,
            3 => Self::ModelUnreadable,
            4 => Self::ModelRejected,
            5 => Self::TooLong,
            6 => Self::Deadline,
            7 => Self::Decode,
            8 => Self::Tokenizer,
            9 => Self::Internal,
            10 => Self::AlreadyLoaded,
            11 => Self::ModelChanged,
            12 => Self::UnknownModel,
            13 => Self::ProfileMismatch,
            14 => Self::Reserved,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// `model` is a registered model ID. The worker applies that model's
    /// profile from its own build; nothing else about a profile is sent.
    Load {
        path: String,
        threads: u8,
        model: String,
    },
    Score {
        sentence: bool,
        id: u64,
        budget_ms: u32,
        job: Job,
    },
    Tokenize {
        id: u64,
        texts: Vec<String>,
    },
    Shutdown,
}

/// What a loaded worker reports about itself. The parent compares every field
/// with the registered model and its profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub model_sha256: [u8; 32],
    pub model_size: u64,
    pub vocab_sha256: [u8; 32],
    pub vocab_size: u32,
    /// Digest of the profile the worker applied, from its own registry.
    pub profile_sha256: [u8; 32],
    pub start: Option<u32>,
    pub end: u32,
    pub context_tokens: u32,
    pub threads: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Hello {
        protocol: u32,
        runtime: String,
        /// Non-CPU compute devices compiled into the runtime; must be zero.
        accelerators: u8,
    },
    Loaded(Loaded),
    /// One entry per candidate, in request order. `None` marks a candidate the
    /// tokenizer cannot represent exactly; it is never ranked.
    Scores {
        id: u64,
        scores: Vec<Option<f32>>,
        tokens: u32,
    },
    /// `None` marks a text the tokenizer cannot reproduce byte for byte.
    Tokens {
        id: u64,
        ids: Vec<Option<Vec<u32>>>,
    },
    Error {
        id: u64,
        fault: Fault,
    },
}

/// The shape of a registered model ID. Anything else is refused as a frame.
pub fn model_id(value: &str) -> bool {
    (1..=MAX_MODEL_ID).contains(&value.len())
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'-'))
}

pub fn read<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    Ok(body)
}
pub fn write<W: Write>(writer: &mut W, body: &[u8]) -> io::Result<()> {
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    writer.write_all(&(body.len() as u32).to_le_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}

struct Out(Vec<u8>);
impl Out {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn text(&mut self, v: &str) {
        self.u16(v.len().min(u16::MAX as usize) as u16);
        self.0.extend_from_slice(v.as_bytes());
    }
}
struct In<'a>(&'a [u8]);
impl<'a> In<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.0.len() < len {
            return None;
        }
        let (head, tail) = self.0.split_at(len);
        self.0 = tail;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2)
            .map(|b| u16::from_le_bytes(b.try_into().unwrap_or_default()))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    }
    fn text(&mut self, max_bytes: usize) -> Option<String> {
        let len = self.u16()? as usize;
        if len > max_bytes {
            return None;
        }
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
    fn done(&self) -> Option<()> {
        self.0.is_empty().then_some(())
    }
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Out(Vec::new());
        match self {
            Self::Load {
                path,
                threads,
                model,
            } => {
                out.u8(1);
                out.text(path);
                out.u8(*threads);
                out.text(model);
            }
            Self::Score {
                id,
                budget_ms,
                job,
                sentence,
            } => {
                out.u8(if *sentence { 5 } else { 2 });
                out.u64(*id);
                out.u32(*budget_ms);
                out.text(&job.context);
                out.text(&job.reading);
                out.text(&job.prefix);
                out.text(&job.suffix);
                out.u8(job.candidates.len().min(255) as u8);
                for candidate in &job.candidates {
                    out.text(candidate);
                }
            }
            Self::Tokenize { id, texts } => {
                out.u8(3);
                out.u64(*id);
                out.u16(texts.len().min(u16::MAX as usize) as u16);
                for text in texts {
                    out.text(text);
                }
            }
            Self::Shutdown => out.u8(4),
        }
        out.0
    }
    /// Refuses anything outside the fixed bounds instead of truncating it.
    pub fn decode(body: &[u8]) -> Option<Self> {
        let mut input = In(body);
        let request = match input.u8()? {
            1 => {
                let path = input.text(MAX_PATH)?;
                let threads = input.u8()?;
                let model = input.text(MAX_MODEL_ID)?;
                if !model_id(&model) {
                    return None;
                }
                Self::Load {
                    path,
                    threads,
                    model,
                }
            }
            op @ (2 | 5) => {
                let id = input.u64()?;
                let budget_ms = input.u32()?;
                let bytes = limits::TEXT_BYTES;
                let context = input.text(bytes)?;
                let reading = input.text(bytes)?;
                let prefix = input.text(bytes)?;
                let suffix = input.text(bytes)?;
                let count = input.u8()? as usize;
                if count > limits::CANDIDATES {
                    return None;
                }
                let mut candidates = Vec::with_capacity(count);
                for _ in 0..count {
                    candidates.push(input.text(bytes)?);
                }
                let job = Job {
                    context,
                    reading,
                    prefix,
                    suffix,
                    candidates,
                };
                if !job.valid() {
                    return None;
                }
                Self::Score {
                    id,
                    budget_ms,
                    job,
                    sentence: op == 5,
                }
            }
            3 => {
                let id = input.u64()?;
                let count = input.u16()? as usize;
                if count > MAX_TOKENIZE_TEXTS {
                    return None;
                }
                let mut texts = Vec::with_capacity(count);
                for _ in 0..count {
                    let text = input.text(MAX_TOKENIZE_CHARS * 4)?;
                    if text.chars().count() > MAX_TOKENIZE_CHARS {
                        return None;
                    }
                    texts.push(text);
                }
                Self::Tokenize { id, texts }
            }
            4 => Self::Shutdown,
            _ => return None,
        };
        input.done()?;
        Some(request)
    }
}

impl Reply {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Out(Vec::new());
        match self {
            Self::Hello {
                protocol,
                runtime,
                accelerators,
            } => {
                out.u8(0x81);
                out.u32(*protocol);
                out.text(runtime);
                out.u8(*accelerators);
            }
            Self::Loaded(loaded) => {
                out.u8(0x82);
                out.0.extend_from_slice(&loaded.model_sha256);
                out.u64(loaded.model_size);
                out.0.extend_from_slice(&loaded.vocab_sha256);
                out.u32(loaded.vocab_size);
                out.0.extend_from_slice(&loaded.profile_sha256);
                match loaded.start {
                    Some(start) => {
                        out.u8(1);
                        out.u32(start);
                    }
                    None => out.u8(0),
                }
                out.u32(loaded.end);
                out.u32(loaded.context_tokens);
                out.u8(loaded.threads);
            }
            Self::Scores { id, scores, tokens } => {
                out.u8(0x83);
                out.u64(*id);
                out.u32(*tokens);
                out.u8(scores.len().min(255) as u8);
                for score in scores {
                    match score {
                        Some(value) => {
                            out.u8(1);
                            out.u32(value.to_bits());
                        }
                        None => out.u8(0),
                    }
                }
            }
            Self::Tokens { id, ids } => {
                out.u8(0x84);
                out.u64(*id);
                out.u16(ids.len().min(u16::MAX as usize) as u16);
                for sequence in ids {
                    let Some(sequence) = sequence else {
                        out.u8(0);
                        continue;
                    };
                    out.u8(1);
                    out.u16(sequence.len().min(u16::MAX as usize) as u16);
                    for token in sequence {
                        out.u32(*token);
                    }
                }
            }
            Self::Error { id, fault } => {
                out.u8(0x8f);
                out.u64(*id);
                out.u8(*fault as u8);
            }
        }
        out.0
    }
    /// Rejects malformed frames and every non-finite score.
    pub fn decode(body: &[u8]) -> Option<Self> {
        let mut input = In(body);
        let reply = match input.u8()? {
            0x81 => Self::Hello {
                protocol: input.u32()?,
                runtime: input.text(128)?,
                accelerators: input.u8()?,
            },
            0x82 => Self::Loaded(Loaded {
                model_sha256: input.take(32)?.try_into().ok()?,
                model_size: input.u64()?,
                vocab_sha256: input.take(32)?.try_into().ok()?,
                vocab_size: input.u32()?,
                profile_sha256: input.take(32)?.try_into().ok()?,
                start: match input.u8()? {
                    0 => None,
                    1 => Some(input.u32()?),
                    _ => return None,
                },
                end: input.u32()?,
                context_tokens: input.u32()?,
                threads: input.u8()?,
            }),
            0x83 => {
                let id = input.u64()?;
                let tokens = input.u32()?;
                let count = input.u8()? as usize;
                if count > limits::CANDIDATES {
                    return None;
                }
                let mut scores = Vec::with_capacity(count);
                for _ in 0..count {
                    scores.push(match input.u8()? {
                        0 => None,
                        1 => {
                            let value = f32::from_bits(input.u32()?);
                            // A log probability is finite and never positive.
                            if !value.is_finite() || value > 0.0 {
                                return None;
                            }
                            Some(value)
                        }
                        _ => return None,
                    });
                }
                Self::Scores { id, scores, tokens }
            }
            0x84 => {
                let id = input.u64()?;
                let count = input.u16()? as usize;
                if count > MAX_TOKENIZE_TEXTS {
                    return None;
                }
                let mut ids = Vec::with_capacity(count);
                for _ in 0..count {
                    match input.u8()? {
                        0 => ids.push(None),
                        1 => {
                            let len = input.u16()? as usize;
                            if len > MAX_TOKEN_IDS {
                                return None;
                            }
                            let mut sequence = Vec::with_capacity(len);
                            for _ in 0..len {
                                sequence.push(input.u32()?);
                            }
                            ids.push(Some(sequence));
                        }
                        _ => return None,
                    }
                }
                Self::Tokens { id, ids }
            }
            0x8f => Self::Error {
                id: input.u64()?,
                fault: Fault::from_u8(input.u8()?)?,
            },
            _ => return None,
        };
        input.done()?;
        Some(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job() -> Job {
        Job {
            context: "きのうは".into(),
            reading: "コウエン".into(),
            prefix: String::new(),
            suffix: "に".into(),
            candidates: vec!["公園".into(), "講演".into()],
        }
    }
    #[test]
    fn requests_and_replies_round_trip() {
        for request in [
            Request::Load {
                path: "/models/model.gguf".into(),
                threads: 4,
                model: "model-1.0-q5".into(),
            },
            Request::Score {
                sentence: false,
                id: 7,
                budget_ms: 500,
                job: job(),
            },
            Request::Score {
                id: 11,
                budget_ms: 500,
                job: job(),
                sentence: true,
            },
            Request::Tokenize {
                id: 8,
                texts: vec!["あ".into(), String::new()],
            },
            Request::Shutdown,
        ] {
            assert_eq!(Request::decode(&request.encode()), Some(request));
        }
        for reply in [
            Reply::Hello {
                protocol: PROTOCOL,
                runtime: "runtime".into(),
                accelerators: 0,
            },
            Reply::Loaded(Loaded {
                model_sha256: [1; 32],
                model_size: 9,
                vocab_sha256: [2; 32],
                vocab_size: 6000,
                profile_sha256: [3; 32],
                start: None,
                end: 3,
                context_tokens: 384,
                threads: 4,
            }),
            Reply::Loaded(Loaded {
                model_sha256: [1; 32],
                model_size: 9,
                vocab_sha256: [2; 32],
                vocab_size: 6000,
                profile_sha256: [4; 32],
                start: Some(2),
                end: 2,
                context_tokens: 384,
                threads: 1,
            }),
            Reply::Error {
                id: 0,
                fault: Fault::ProfileMismatch,
            },
            Reply::Error {
                id: 9,
                fault: Fault::Reserved,
            },
            Reply::Scores {
                id: 7,
                scores: vec![Some(-1.5), None],
                tokens: 12,
            },
            Reply::Tokens {
                id: 8,
                ids: vec![Some(vec![1, 2]), None, Some(vec![])],
            },
            Reply::Error {
                id: 7,
                fault: Fault::Deadline,
            },
        ] {
            assert_eq!(Reply::decode(&reply.encode()), Some(reply));
        }
    }
    #[test]
    fn malformed_oversized_and_nonfinite_frames_are_refused() {
        let good = Request::Score {
            sentence: false,
            id: 1,
            budget_ms: 1,
            job: job(),
        }
        .encode();
        assert!(Request::decode(&good[..good.len() - 1]).is_none());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(Request::decode(&trailing).is_none());
        let mut too_many = job();
        too_many.candidates = vec!["あ".into(); limits::CANDIDATES + 1];
        assert!(
            Request::decode(
                &Request::Score {
                    sentence: false,
                    id: 1,
                    budget_ms: 1,
                    job: too_many
                }
                .encode()
            )
            .is_none()
        );
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.5] {
            let mut frame = vec![0x83];
            frame.extend_from_slice(&1u64.to_le_bytes());
            frame.extend_from_slice(&3u32.to_le_bytes());
            frame.push(1);
            frame.push(1);
            frame.extend_from_slice(&bad.to_bits().to_le_bytes());
            assert!(Reply::decode(&frame).is_none());
        }
        assert!(Reply::decode(&[0x8f, 0, 0, 0, 0, 0, 0, 0, 0, 99]).is_none());
        // A load request must name a model, and only in the shape of an ID.
        for model in ["", "Model", "a/b", "a b", "../x", &"a".repeat(65)] {
            let request = Request::Load {
                path: "/models/model.gguf".into(),
                threads: 4,
                model: model.into(),
            };
            assert!(Request::decode(&request.encode()).is_none(), "{model}");
        }
        let load = Request::Load {
            path: "/models/model.gguf".into(),
            threads: 4,
            model: "m".into(),
        }
        .encode();
        // The previous protocol's frame, without a model, is refused.
        assert!(Request::decode(&load[..load.len() - 3]).is_none());
        // An unknown start-token tag is refused.
        let mut loaded = Reply::Loaded(Loaded {
            model_sha256: [1; 32],
            model_size: 9,
            vocab_sha256: [2; 32],
            vocab_size: 6000,
            profile_sha256: [3; 32],
            start: None,
            end: 3,
            context_tokens: 320,
            threads: 4,
        })
        .encode();
        loaded[1 + 32 + 8 + 32 + 4 + 32] = 2;
        assert!(Reply::decode(&loaded).is_none());
        assert!(Reply::decode(&[]).is_none());
        assert!(read(&mut &[0u8, 0, 0, 0][..]).is_err());
        assert!(read(&mut &((MAX_FRAME as u32 + 1).to_le_bytes())[..]).is_err());
    }
}
