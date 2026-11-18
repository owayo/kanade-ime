//! Offline export of the segments and candidates the locked worker returns.
//!
//! Requests and responses are JSON Lines. A response is the only place where
//! supplied text may appear; diagnostics name a row and a fixed reason.
use crate::{key, romaji, special};
use anyhow::{Context, Result, bail};
use kanade_ipc::{MAX_CONTEXT, Preedit, Privacy, SpecialKey};
use kanade_mozc::{ConversionEngine, FocusCandidate, Mozc, SessionId, Snapshot, WorkerIdentity};
use serde::Serialize;
use std::{
    collections::HashSet,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const SCHEMA: &str = "kanade-candidates/v1";
const MAX_REQUEST: usize = 64 * 1024;
const USAGE: &str =
    "usage: kanade-eval candidates [--limit N] [--max-reading N] [--deadline-ms N] [--suggest]";

/// The segments of one conversion and its whole surface.
type Conversion = (Vec<Segment>, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Candidates kept for each segment.
    limit: usize,
    /// Longest accepted reading, in characters.
    max_reading: usize,
    /// How long the worker may take over one reply. The interactive timeout
    /// would fail long conversions, and any conversion on a busy machine;
    /// here only a worker that stopped answering should fail.
    deadline_ms: usize,
    /// Ask for suggestions on every key, as interactive input does. They do
    /// not change a response and make a long reading far slower to type.
    suggest: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            limit: 50,
            max_reading: 64,
            deadline_ms: 10_000,
            suggest: false,
        }
    }
}
impl Options {
    pub fn parse(arguments: &[String]) -> Result<Self> {
        let mut options = Self::default();
        let mut arguments = arguments.iter();
        while let Some(argument) = arguments.next() {
            if argument == "--suggest" {
                options.suggest = true;
                continue;
            }
            let (name, inline) = match argument.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (argument.as_str(), None),
            };
            let setting = match name {
                "--limit" => &mut options.limit,
                "--max-reading" => &mut options.max_reading,
                "--deadline-ms" => &mut options.deadline_ms,
                _ => bail!(USAGE),
            };
            let value = match inline {
                Some(value) => value,
                None => arguments.next().context(USAGE)?,
            };
            *setting = match value.parse() {
                Ok(number) if number > 0 => number,
                _ => bail!("{name} takes a positive integer"),
            };
        }
        Ok(options)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Fault {
    BadRequest,
    UnsupportedReading,
    ReadingMismatch,
    EarlyCommit,
    NoConversion,
    Focus,
    Engine,
}

/// A row either fails by its own content or because the worker failed.
enum Failure {
    Row(Fault),
    Worker(kanade_mozc::Error),
}
impl From<Fault> for Failure {
    fn from(fault: Fault) -> Self {
        Self::Row(fault)
    }
}
impl From<kanade_mozc::Error> for Failure {
    fn from(error: kanade_mozc::Error) -> Self {
        Self::Worker(error)
    }
}

#[derive(Serialize)]
struct Header<'a> {
    schema: &'static str,
    mozc: Upstream<'a>,
    worker: Artifact<'a>,
    kanade_commit: Option<&'static str>,
    limit: usize,
    privacy: &'static str,
    profile: &'static str,
}
#[derive(Serialize)]
struct Upstream<'a> {
    tag: &'a str,
    commit: &'a str,
    data_version: u32,
    engine_version: u32,
}
#[derive(Serialize)]
struct Artifact<'a> {
    sha256: &'a str,
    protocol: u32,
}

#[derive(Debug, PartialEq, Serialize)]
struct Candidate {
    value: String,
    transliteration: bool,
}
#[derive(Debug, PartialEq, Serialize)]
struct Segment {
    reading: String,
    candidates: Vec<Candidate>,
    total: usize,
}
#[derive(Debug, PartialEq, Serialize)]
#[serde(untagged)]
enum Response {
    Converted {
        id: String,
        ok: bool,
        reading: String,
        segments: Vec<Segment>,
        top1: String,
    },
    Failed {
        id: Option<String>,
        ok: bool,
        error: Fault,
    },
}
impl Response {
    fn failed(id: Option<String>, error: Fault) -> Self {
        Self::Failed {
            id,
            ok: false,
            error,
        }
    }
}

struct Request {
    id: String,
    reading: String,
    preceding: String,
}
/// A rejected line still reports its identifier when that much is readable.
fn parse(line: &[u8]) -> Result<Request, Option<String>> {
    let Ok(serde_json::Value::Object(mut fields)) = serde_json::from_slice(line) else {
        return Err(None);
    };
    let Some(serde_json::Value::String(id)) = fields.remove("id") else {
        return Err(None);
    };
    let reading = fields.remove("reading");
    // The engine refuses a longer context, so the worker never receives one.
    let preceding = match fields.remove("preceding") {
        None => Some(String::new()),
        Some(serde_json::Value::String(text)) if text.len() <= MAX_CONTEXT => Some(text),
        Some(_) => None,
    };
    match (reading, preceding) {
        // An unknown field is more likely a misspelt one than extra data.
        (Some(serde_json::Value::String(reading)), Some(preceding)) if fields.is_empty() => {
            Ok(Request {
                id,
                reading,
                preceding,
            })
        }
        _ => Err(Some(id)),
    }
}

/// The order the engine displays: worker order, first of each surface only.
fn displayed(candidates: &[FocusCandidate], limit: usize) -> (Vec<Candidate>, usize) {
    let mut surfaces = HashSet::new();
    let mut shown = Vec::new();
    for candidate in candidates {
        if surfaces.insert(candidate.value.as_str()) && shown.len() < limit {
            shown.push(Candidate {
                value: candidate.value.clone(),
                transliteration: candidate.transliteration,
            });
        }
    }
    (shown, surfaces.len())
}

/// The focus is on `index` and every segment is still as first converted.
fn focused_on(snapshot: &Snapshot, converted: &Preedit, index: usize) -> bool {
    snapshot.consumed
        && snapshot.commit.is_none()
        && snapshot.converting
        && snapshot.focused_segment == index
        && snapshot.preedit.segments.len() == converted.segments.len()
        && snapshot
            .preedit
            .segments
            .iter()
            .zip(&converted.segments)
            .all(|(now, first)| now.reading == first.reading && now.value == first.value)
}

/// Type the reading, convert once and read each segment's candidates by moving
/// the focus. Moving the focus selects nothing, so the first conversion stays.
fn walk(
    engine: &mut impl ConversionEngine,
    session: SessionId,
    reading: &str,
    keys: &str,
    preceding: &str,
    limit: usize,
) -> Result<Conversion, Failure> {
    let mut typed = Snapshot::default();
    for c in keys.chars() {
        typed = engine.key(session, &key(c), preceding)?;
        if typed.commit.is_some() || !typed.consumed {
            return Err(Fault::EarlyCommit.into());
        }
    }
    if kanade_ipc::reading(&typed.preedit) != reading {
        return Err(Fault::ReadingMismatch.into());
    }
    let converted = engine.key(session, &special(SpecialKey::Space), preceding)?;
    if converted.commit.is_some() || !converted.consumed {
        return Err(Fault::EarlyCommit.into());
    }
    if !converted.converting {
        return Err(Fault::NoConversion.into());
    }
    // Segment readings are returned to the caller as parts of the request.
    if kanade_ipc::reading(&converted.preedit) != reading {
        return Err(Fault::ReadingMismatch.into());
    }
    let mut segments = Vec::with_capacity(converted.preedit.segments.len());
    let mut moved = None;
    for (index, segment) in converted.preedit.segments.iter().enumerate() {
        if index > 0 {
            moved = Some(engine.key(session, &special(SpecialKey::Right), preceding)?);
        }
        let focused = moved.as_ref().unwrap_or(&converted);
        if !focused_on(focused, &converted.preedit, index) {
            return Err(Fault::Focus.into());
        }
        let (candidates, total) = displayed(&focused.candidates, limit);
        if candidates.first().map(|top| top.value.as_str()) != Some(segment.value.as_str()) {
            return Err(Fault::Focus.into());
        }
        segments.push(Segment {
            reading: segment.reading.clone(),
            candidates,
            total,
        });
    }
    Ok((segments, kanade_ipc::surface(&converted.preedit)))
}

/// One request in its own session, which is ended whatever the walk found.
/// A failed worker is not asked to clean up; its owner discards it instead.
fn in_session(
    engine: &mut impl ConversionEngine,
    reading: &str,
    keys: &str,
    preceding: &str,
    limit: usize,
) -> Result<Conversion, Failure> {
    let session = engine.create_session()?;
    let walked = walk(engine, session, reading, keys, preceding, limit);
    if matches!(walked, Err(Failure::Worker(_))) {
        return walked;
    }
    engine.reset(session)?;
    engine.delete_session(session)?;
    walked
}

/// A worker on its own empty profile, with learning disabled.
struct Backend {
    mozc: Mozc,
    // Removed after the worker above has exited.
    _profile: tempfile::TempDir,
}
impl Backend {
    fn start(
        worker: &Path,
        expected: Option<&WorkerIdentity>,
        options: &Options,
    ) -> kanade_mozc::Result<Self> {
        let profile = tempfile::tempdir()?;
        let mut mozc = Mozc::start(worker, profile.path())?;
        // The header names one worker; a replacement must be that worker.
        if expected.is_some_and(|identity| identity != mozc.identity()) {
            return Err(kanade_mozc::Error::Version);
        }
        mozc.set_request_timeout(Duration::from_millis(options.deadline_ms as u64));
        mozc.set_suggestions(options.suggest);
        mozc.set_privacy(Privacy::Private)?;
        Ok(Self {
            mozc,
            _profile: profile,
        })
    }
}

struct Exporter {
    worker: PathBuf,
    identity: WorkerIdentity,
    options: Options,
    backend: Option<Backend>,
}
impl Exporter {
    fn start(options: Options) -> kanade_mozc::Result<Self> {
        let worker = kanade_mozc::default_worker();
        let backend = Backend::start(&worker, None, &options)?;
        Ok(Self {
            identity: backend.mozc.identity().clone(),
            worker,
            options,
            backend: Some(backend),
        })
    }
    fn header(&self) -> Header<'_> {
        Header {
            schema: SCHEMA,
            mozc: Upstream {
                tag: &self.identity.tag,
                commit: &self.identity.commit,
                data_version: self.identity.data_version,
                engine_version: self.identity.engine_version,
            },
            worker: Artifact {
                sha256: &self.identity.sha256,
                protocol: self.identity.protocol,
            },
            kanade_commit: option_env!("KANADE_COMMIT").filter(|commit| !commit.is_empty()),
            limit: self.options.limit,
            privacy: "private",
            profile: "fresh",
        }
    }
    fn attempt(
        &mut self,
        reading: &str,
        keys: &str,
        preceding: &str,
    ) -> Result<Conversion, Failure> {
        let backend = match &mut self.backend {
            Some(backend) => backend,
            vacant => vacant.insert(Backend::start(
                &self.worker,
                Some(&self.identity),
                &self.options,
            )?),
        };
        in_session(
            &mut backend.mozc,
            reading,
            keys,
            preceding,
            self.options.limit,
        )
    }
    /// A worker failure is not a property of the row: the row is tried once
    /// more on a new worker before it is reported as failed.
    fn convert(
        &mut self,
        row: u64,
        reading: &str,
        keys: &str,
        preceding: &str,
    ) -> Result<Conversion, Fault> {
        let mut retried = false;
        loop {
            let error = match self.attempt(reading, keys, preceding) {
                Ok(conversion) => return Ok(conversion),
                Err(Failure::Row(fault)) => return Err(fault),
                Err(Failure::Worker(error)) => error,
            };
            // Its state is unknown: the next attempt or request starts a new
            // worker on a new profile instead of reusing this one.
            self.backend = None;
            let next = if retried {
                "reporting the row as engine"
            } else {
                "retrying the row on a new worker"
            };
            // Best effort: a closed standard error must not cost a row.
            let _ = writeln!(
                std::io::stderr(),
                "kanade-eval candidates: row {row}: {error}; {next}"
            );
            if retried {
                return Err(Fault::Engine);
            }
            retried = true;
        }
    }
    fn respond(&mut self, row: u64, line: &[u8]) -> Response {
        let request = match parse(line) {
            Ok(request) => request,
            Err(id) => return Response::failed(id, Fault::BadRequest),
        };
        let reading = romaji::hiragana(&request.reading);
        let keys = (1..=self.options.max_reading)
            .contains(&reading.chars().count())
            .then(|| romaji::keys(&reading))
            .flatten();
        let converted = match keys {
            Some(keys) => self.convert(row, &reading, &keys, &request.preceding),
            None => Err(Fault::UnsupportedReading),
        };
        match converted {
            Ok((segments, top1)) => Response::Converted {
                id: request.id,
                ok: true,
                reading,
                segments,
                top1,
            },
            Err(fault) => Response::failed(Some(request.id), fault),
        }
    }
}

fn emit(output: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    output.write_all(&line)?;
    // A caller may wait for each response before sending the next request.
    output.flush()?;
    Ok(())
}

/// Answer every input line, in order, with exactly one output line. Only a
/// failure to read the requests or to write a response ends the run early.
fn serve(exporter: &mut Exporter, mut input: impl BufRead, mut output: impl Write) -> Result<()> {
    emit(&mut output, &exporter.header())?;
    let mut line = Vec::new();
    let mut row = 0;
    loop {
        line.clear();
        if std::io::Read::take(&mut input, (MAX_REQUEST + 1) as u64).read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(());
        }
        row += 1;
        if line.len() > MAX_REQUEST {
            // Drain this row without growing the buffer, then resume at the next.
            if !line.ends_with(b"\n") {
                loop {
                    let bytes = input.fill_buf()?;
                    if bytes.is_empty() {
                        break;
                    }
                    let end = bytes.iter().position(|byte| *byte == b'\n');
                    let count = end.map_or(bytes.len(), |index| index + 1);
                    input.consume(count);
                    if end.is_some() {
                        break;
                    }
                }
            }
            emit(&mut output, &Response::failed(None, Fault::BadRequest))?;
            continue;
        }
        emit(&mut output, &exporter.respond(row, &line))?;
    }
}

pub fn run(options: Options, input: impl BufRead, output: impl Write) -> Result<()> {
    // The header names the worker, so nothing is written before one runs.
    let mut exporter = Exporter::start(options).context("the locked worker did not start")?;
    serve(&mut exporter, input, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn request(id: &str, reading: &str, preceding: &str) -> String {
        json!({"id": id, "reading": reading, "preceding": preceding}).to_string()
    }
    fn options(arguments: &[&str]) -> Result<Options> {
        let arguments: Vec<_> = arguments.iter().map(|a| (*a).to_owned()).collect();
        Options::parse(&arguments)
    }
    /// The header followed by one parsed response per output line.
    fn lines(output: &[u8]) -> (Value, Vec<Value>) {
        let mut lines = std::str::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap());
        (lines.next().unwrap(), lines.collect())
    }
    fn export(arguments: &[&str], input: impl AsRef<[u8]>) -> (Value, Vec<Value>) {
        let mut output = Vec::new();
        run(options(arguments).unwrap(), input.as_ref(), &mut output).unwrap();
        lines(&output)
    }
    fn values(segment: &Value) -> Vec<&str> {
        segment["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|candidate| candidate["value"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn oversized_rows_are_bounded_and_do_not_consume_the_next_request() {
        let nowhere = tempfile::tempdir().unwrap();
        let mut exporter = Exporter {
            worker: nowhere.path().join("missing"),
            identity: WorkerIdentity {
                tag: "fixture".into(),
                commit: "fixture".into(),
                data_version: 1,
                engine_version: 1,
                sha256: "fixture".into(),
                protocol: 2,
            },
            options: Options::default(),
            backend: None,
        };
        let input = format!(
            "{}\n{}\n{}",
            "x".repeat(MAX_REQUEST * 5),
            request("next", "漢字", ""),
            "x".repeat(MAX_REQUEST + 1)
        );
        let mut output = vec![];
        serve(&mut exporter, input.as_bytes(), &mut output).unwrap();
        let (_, rows) = lines(&output);
        assert_eq!(
            rows,
            vec![
                json!({"id":null,"ok":false,"error":"bad_request"}),
                json!({"id":"next","ok":false,"error":"unsupported_reading"}),
                json!({"id":null,"ok":false,"error":"bad_request"})
            ]
        );
    }
    #[test]
    fn requests_are_validated_before_any_conversion() {
        let parsed = parse(request("a", "かな", "前").as_bytes()).unwrap();
        assert_eq!(
            (parsed.id, parsed.reading, parsed.preceding),
            ("a".to_owned(), "かな".to_owned(), "前".to_owned())
        );
        // A missing context is an empty one; line endings are not content.
        let parsed = parse(b"{\"id\":\"\",\"reading\":\"\"}\r\n").unwrap();
        assert_eq!(
            (parsed.id, parsed.preceding),
            (String::new(), String::new())
        );
        // The context is bounded in bytes, like the engine's.
        let context = "a".repeat(MAX_CONTEXT);
        assert!(parse(request("a", "かな", &context).as_bytes()).is_ok());

        for line in [
            &b""[..],
            b"\n",
            b"not json",
            b"[1,2]",
            b"\"text\"",
            br#"{"reading":"a"}"#,
            br#"{"id":1,"reading":"a"}"#,
            br#"{"id":null,"reading":"a"}"#,
            b"{\"id\":\"a\",\"reading\":\"\xff\"}",
            br#"{"id":"a","reading":"a"} {"id":"b","reading":"b"}"#,
        ] {
            assert_eq!(parse(line).err(), Some(None));
        }
        let long = request("a", "かな", &format!("{context}a"));
        let wide = request("a", "かな", &"あ".repeat(MAX_CONTEXT / 3 + 1));
        for line in [
            r#"{"id":"a"}"#,
            r#"{"id":"a","reading":1}"#,
            r#"{"id":"a","reading":null,"preceding":""}"#,
            r#"{"id":"a","reading":"a","preceding":null}"#,
            r#"{"id":"a","reading":"a","preceding":["x"]}"#,
            r#"{"id":"a","reading":"a","preceeding":"x"}"#,
            long.as_str(),
            wide.as_str(),
        ] {
            assert_eq!(parse(line.as_bytes()).err(), Some(Some("a".to_owned())));
        }
    }

    #[test]
    fn options_have_defaults_and_reject_anything_else() {
        let parsed = |arguments: &[&str]| {
            let options = options(arguments).unwrap();
            (options.limit, options.max_reading, options.deadline_ms)
        };
        assert_eq!(parsed(&[]), (50, 64, 10_000));
        assert_eq!(parsed(&["--limit", "12"]), (12, 64, 10_000));
        assert_eq!(parsed(&["--limit=7"]), (7, 64, 10_000));
        assert_eq!(parsed(&["--max-reading", "200"]), (50, 200, 10_000));
        assert_eq!(parsed(&["--deadline-ms", "30000"]), (50, 64, 30_000));
        assert_eq!(
            parsed(&["--max-reading=1", "--deadline-ms=5", "--limit", "3"]),
            (3, 1, 5)
        );
        assert_eq!(parsed(&["--limit", "3", "--limit", "4"]), (4, 64, 10_000));
        // Suggestions are asked for only on request.
        assert!(!options(&[]).unwrap().suggest);
        assert!(!options(&["--limit", "3"]).unwrap().suggest);
        let suggesting = options(&["--limit", "3", "--suggest", "--max-reading=9"]).unwrap();
        assert!(suggesting.suggest);
        assert_eq!((suggesting.limit, suggesting.max_reading), (3, 9));
        for rejected in [
            &["--suggest=1"][..],
            &["--suggest", "1"],
            &["--no-suggest"],
            &["--deadline-ms"],
            &["--deadline-ms", "0"],
            &["--deadline-ms", "1.5"],
            &["--limit"],
            &["--limit", "0"],
            &["--limit", "-1"],
            &["--limit", "many"],
            &["--limit="],
            &["--limit", "3", "4"],
            &["--max-reading"],
            &["--max-reading", "0"],
            &["--max-reading=64", "--limit"],
            &["12"],
            &["--other", "12"],
        ] {
            assert!(options(rejected).is_err());
        }
    }

    fn candidate(value: &str, transliteration: bool) -> FocusCandidate {
        FocusCandidate {
            value: value.to_owned(),
            reading: String::new(),
            annotation: String::new(),
            transliteration,
            history_deletable: false,
        }
    }
    fn part(reading: &str, value: &str, focused: bool) -> kanade_ipc::Segment {
        kanade_ipc::Segment {
            reading: reading.to_owned(),
            value: value.to_owned(),
            focused,
            value_utf16: kanade_ipc::utf16_len(value),
        }
    }
    fn composing(text: &str) -> Snapshot {
        Snapshot {
            consumed: true,
            preedit: Preedit {
                segments: vec![part(text, text, false)],
                ..Default::default()
            },
            ..Default::default()
        }
    }
    /// A conversion with the focus on one segment and that segment's candidates.
    fn converting(parts: &[(&str, &str)], focused: usize, candidates: &[&str]) -> Snapshot {
        Snapshot {
            consumed: true,
            converting: true,
            focused_segment: focused,
            preedit: Preedit {
                segments: parts
                    .iter()
                    .enumerate()
                    .map(|(index, (reading, value))| part(reading, value, index == focused))
                    .collect(),
                ..Default::default()
            },
            candidates: candidates
                .iter()
                .map(|value| candidate(value, false))
                .collect(),
            ..Default::default()
        }
    }
    /// Replays prepared replies, one for each key, and records what was sent.
    #[derive(Default)]
    struct Scripted {
        replies: std::collections::VecDeque<kanade_mozc::Result<Snapshot>>,
        sent: Vec<(kanade_ipc::Key, String)>,
        /// Session calls in order; keys are in `sent`.
        calls: Vec<&'static str>,
        /// A session call that fails, as it would on a broken worker.
        broken: Option<&'static str>,
    }
    impl Scripted {
        fn call(&mut self, name: &'static str) -> kanade_mozc::Result<()> {
            self.calls.push(name);
            if self.broken == Some(name) {
                return Err(kanade_mozc::Error::Protocol);
            }
            Ok(())
        }
    }
    impl ConversionEngine for Scripted {
        fn add_history(&mut self, _: &str, _: &str) -> kanade_mozc::Result<()> {
            Ok(())
        }
        fn create_session(&mut self) -> kanade_mozc::Result<SessionId> {
            self.call("create")?;
            Ok(SessionId::scripted(1))
        }
        fn delete_session(&mut self, _: SessionId) -> kanade_mozc::Result<()> {
            self.call("delete")
        }
        fn set_privacy(&mut self, _: Privacy) -> kanade_mozc::Result<()> {
            Ok(())
        }
        fn key(
            &mut self,
            _: SessionId,
            key: &kanade_ipc::Key,
            preceding: &str,
        ) -> kanade_mozc::Result<Snapshot> {
            self.sent.push((key.clone(), preceding.to_owned()));
            self.replies
                .pop_front()
                .unwrap_or(Err(kanade_mozc::Error::Protocol))
        }
        fn select(&mut self, _: SessionId, _: &str) -> kanade_mozc::Result<Snapshot> {
            Err(kanade_mozc::Error::StaleCandidate)
        }
        fn delete_candidate_history(
            &mut self,
            _: SessionId,
            _: &str,
        ) -> kanade_mozc::Result<Snapshot> {
            Err(kanade_mozc::Error::StaleCandidate)
        }
        fn submit(&mut self, _: SessionId) -> kanade_mozc::Result<Snapshot> {
            Err(kanade_mozc::Error::Protocol)
        }
        fn reset(&mut self, _: SessionId) -> kanade_mozc::Result<()> {
            self.call("reset")
        }
        fn undo_learning(&mut self, _: SessionId) -> kanade_mozc::Result<()> {
            Ok(())
        }
        fn sync(&mut self) -> kanade_mozc::Result<()> {
            Ok(())
        }
    }
    const PARTS: [(&str, &str); 2] = [("か", "蚊"), ("き", "木")];
    /// The replies of a worker that converts "かき" into two segments.
    fn script() -> Vec<kanade_mozc::Result<Snapshot>> {
        vec![
            Ok(composing("ｋ")),
            Ok(composing("か")),
            Ok(composing("かｋ")),
            Ok(composing("かき")),
            Ok(converting(&PARTS, 0, &["蚊", "か", "蚊", "課"])),
            Ok(converting(&PARTS, 1, &["木", "気", "機"])),
        ]
    }
    /// `None` stands for a worker failure, which has no fixed word of its own.
    fn reason(failure: Failure) -> Option<Fault> {
        match failure {
            Failure::Row(fault) => Some(fault),
            Failure::Worker(_) => None,
        }
    }
    fn walked(
        replies: Vec<kanade_mozc::Result<Snapshot>>,
    ) -> (Result<Conversion, Option<Fault>>, Scripted) {
        let mut engine = Scripted {
            replies: replies.into(),
            ..Default::default()
        };
        let walked = walk(&mut engine, SessionId::scripted(1), "かき", "kaki", "前", 2);
        (walked.map_err(reason), engine)
    }
    /// The outcome of one request and the session calls it made.
    fn session(
        replies: Vec<kanade_mozc::Result<Snapshot>>,
        broken: Option<&'static str>,
    ) -> (Result<(), Option<Fault>>, Vec<&'static str>) {
        let mut engine = Scripted {
            replies: replies.into(),
            broken,
            ..Default::default()
        };
        let outcome = in_session(&mut engine, "かき", "kaki", "前", 2);
        (outcome.map(drop).map_err(reason), engine.calls)
    }
    fn fault(position: usize, reply: kanade_mozc::Result<Snapshot>) -> Option<Fault> {
        let mut replies = script();
        replies[position] = reply;
        walked(replies).0.unwrap_err()
    }

    #[test]
    fn walk_reads_each_segment_after_moving_the_focus() {
        let (walked, engine) = walked(script());
        let shown = |values: &[&str]| -> Vec<Candidate> {
            values
                .iter()
                .map(|value| Candidate {
                    value: (*value).to_owned(),
                    transliteration: false,
                })
                .collect()
        };
        assert_eq!(
            walked,
            Ok((
                vec![
                    Segment {
                        reading: "か".into(),
                        candidates: shown(&["蚊", "か"]),
                        total: 3,
                    },
                    Segment {
                        reading: "き".into(),
                        candidates: shown(&["木", "気"]),
                        total: 3,
                    },
                ],
                "蚊木".to_owned()
            ))
        );
        // Every key carries the context; nothing follows the last segment.
        let sent: Vec<_> = engine
            .sent
            .iter()
            .map(|(key, preceding)| (key.text.as_str(), key.special, preceding.as_str()))
            .collect();
        let none = SpecialKey::None as i32;
        assert_eq!(
            sent,
            [
                ("k", none, "前"),
                ("a", none, "前"),
                ("k", none, "前"),
                ("i", none, "前"),
                ("", SpecialKey::Space as i32, "前"),
                ("", SpecialKey::Right as i32, "前"),
            ]
        );
    }

    #[test]
    fn walk_names_what_went_wrong() {
        let committed = |mut snapshot: Snapshot| {
            snapshot.commit = Some(kanade_ipc::Commit::default());
            Ok(snapshot)
        };
        let passed_through = |mut snapshot: Snapshot| {
            snapshot.consumed = false;
            Ok(snapshot)
        };
        let early = Some(Fault::EarlyCommit);
        assert_eq!(fault(1, committed(composing("か"))), early);
        assert_eq!(fault(2, passed_through(composing("かｋ"))), early);
        assert_eq!(fault(4, committed(converting(&PARTS, 0, &["蚊"]))), early);
        assert_eq!(
            fault(4, passed_through(converting(&PARTS, 0, &["蚊"]))),
            early
        );

        let mismatch = Some(Fault::ReadingMismatch);
        assert_eq!(fault(3, Ok(composing("かｋ"))), mismatch);
        assert_eq!(fault(3, Ok(composing("かぎ"))), mismatch);
        let other = [("か", "蚊"), ("ぎ", "木")];
        assert_eq!(fault(4, Ok(converting(&other, 0, &["蚊"]))), mismatch);

        assert_eq!(fault(4, Ok(composing("かき"))), Some(Fault::NoConversion));

        let focus = Some(Fault::Focus);
        // The first segment is not focused, or its list is not its own.
        assert_eq!(fault(4, Ok(converting(&PARTS, 1, &["蚊"]))), focus);
        assert_eq!(fault(4, Ok(converting(&PARTS, 0, &["か", "蚊"]))), focus);
        assert_eq!(fault(4, Ok(converting(&PARTS, 0, &[]))), focus);
        // The focus did not move, or moving it changed the conversion.
        assert_eq!(fault(5, Ok(converting(&PARTS, 0, &["蚊"]))), focus);
        let changed = [("か", "課"), ("き", "木")];
        assert_eq!(fault(5, Ok(converting(&changed, 1, &["木"]))), focus);
        let merged = [("かき", "柿")];
        assert_eq!(fault(5, Ok(converting(&merged, 0, &["柿"]))), focus);
        assert_eq!(fault(5, Ok(composing("かき"))), focus);
        assert_eq!(fault(5, committed(converting(&PARTS, 1, &["木"]))), focus);
        assert_eq!(fault(5, Ok(converting(&PARTS, 1, &["気", "木"]))), focus);

        // A worker failure is not a property of the row.
        for position in 0..script().len() {
            assert_eq!(fault(position, Err(kanade_mozc::Error::Timeout)), None);
        }
        let mut short = script();
        short.pop();
        assert_eq!(walked(short).0, Err(None));
    }

    #[test]
    fn a_session_is_ended_unless_the_worker_failed() {
        let ended = vec!["create", "reset", "delete"];
        assert_eq!(session(script(), None), (Ok(()), ended.clone()));
        // A row that fails by its own content leaves no session behind.
        let mut mismatched = script();
        mismatched[3] = Ok(composing("かぎ"));
        assert_eq!(
            session(mismatched, None),
            (Err(Some(Fault::ReadingMismatch)), ended.clone())
        );
        // A failed worker is discarded by its owner, not asked to clean up.
        let mut failed = script();
        failed[4] = Err(kanade_mozc::Error::Timeout);
        assert_eq!(session(failed, None), (Err(None), vec!["create"]));
        assert_eq!(
            session(script(), Some("create")),
            (Err(None), vec!["create"])
        );
        // Ending the session is part of the request: nothing is reported from
        // a worker that could not be left clean.
        assert_eq!(
            session(script(), Some("reset")),
            (Err(None), vec!["create", "reset"])
        );
        assert_eq!(session(script(), Some("delete")), (Err(None), ended));
    }

    #[test]
    fn a_worker_that_cannot_start_still_answers_every_line() {
        let nowhere = tempfile::tempdir().unwrap();
        let mut exporter = Exporter {
            worker: nowhere.path().join("missing-worker"),
            identity: WorkerIdentity {
                tag: "1.2.3".into(),
                commit: "abc".into(),
                data_version: 4,
                engine_version: 5,
                sha256: "def".into(),
                protocol: 6,
            },
            options: options(&["--limit", "7"]).unwrap(),
            backend: None,
        };
        let input = [
            request("a", "かな", ""),
            "not json".to_owned(),
            request("b", "漢字", ""),
            request("c", "かな", ""),
        ]
        .join("\n");
        let mut output = Vec::new();
        serve(&mut exporter, input.as_bytes(), &mut output).unwrap();

        // The header keeps the documented fields in the documented order.
        let text = std::str::from_utf8(&output).unwrap();
        let commit = serde_json::to_string(
            &option_env!("KANADE_COMMIT").filter(|commit| !commit.is_empty()),
        )
        .unwrap();
        assert_eq!(
            text.lines().next().unwrap(),
            format!(
                r#"{{"schema":"kanade-candidates/v1","mozc":{{"tag":"1.2.3","commit":"abc","data_version":4,"engine_version":5}},"worker":{{"sha256":"def","protocol":6}},"kanade_commit":{commit},"limit":7,"privacy":"private","profile":"fresh"}}"#
            )
        );
        // Each request tries a new worker; none of them takes a line away.
        let (_, rows) = lines(&output);
        assert_eq!(
            rows,
            [
                json!({"id": "a", "ok": false, "error": "engine"}),
                json!({"id": null, "ok": false, "error": "bad_request"}),
                json!({"id": "b", "ok": false, "error": "unsupported_reading"}),
                json!({"id": "c", "ok": false, "error": "engine"}),
            ]
        );
    }

    #[test]
    fn repeated_surfaces_are_removed_before_the_limit() {
        let list = [
            candidate("日本", false),
            candidate("にほん", false),
            candidate("日本", false),
            candidate("ニホン", false),
            candidate("にほん", true),
            candidate("nihon", true),
        ];
        let shown = |limit| {
            let (shown, total) = displayed(&list, limit);
            let shown: Vec<_> = shown
                .into_iter()
                .map(|c| (c.value, c.transliteration))
                .collect();
            (shown, total)
        };
        let all = vec![
            ("日本".to_owned(), false),
            ("にほん".to_owned(), false),
            ("ニホン".to_owned(), false),
            ("nihon".to_owned(), true),
        ];
        assert_eq!(shown(50), (all.clone(), 4));
        assert_eq!(shown(4), (all.clone(), 4));
        assert_eq!(shown(3), (all[..3].to_vec(), 4));
        assert_eq!(shown(1), (all[..1].to_vec(), 4));
        assert_eq!(displayed(&[], 50), (vec![], 0));
    }

    #[test]
    fn responses_keep_the_documented_shape() {
        let converted = Response::Converted {
            id: "1".into(),
            ok: true,
            reading: "にほん".into(),
            segments: vec![Segment {
                reading: "にほん".into(),
                candidates: vec![Candidate {
                    value: "日本".into(),
                    transliteration: false,
                }],
                total: 3,
            }],
            top1: "日本".into(),
        };
        assert_eq!(
            serde_json::to_string(&converted).unwrap(),
            r#"{"id":"1","ok":true,"reading":"にほん","segments":[{"reading":"にほん","candidates":[{"value":"日本","transliteration":false}],"total":3}],"top1":"日本"}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::failed(None, Fault::BadRequest)).unwrap(),
            r#"{"id":null,"ok":false,"error":"bad_request"}"#
        );
        let words = [
            (Fault::BadRequest, "bad_request"),
            (Fault::UnsupportedReading, "unsupported_reading"),
            (Fault::ReadingMismatch, "reading_mismatch"),
            (Fault::EarlyCommit, "early_commit"),
            (Fault::NoConversion, "no_conversion"),
            (Fault::Focus, "focus"),
            (Fault::Engine, "engine"),
        ];
        for (fault, word) in words {
            assert_eq!(
                serde_json::to_value(Response::failed(Some("x".into()), fault)).unwrap(),
                json!({"id": "x", "ok": false, "error": word})
            );
        }
    }

    fn line(id: &str, reading: &str, preceding: &str) -> String {
        format!("{}\n", request(id, reading, preceding))
    }
    fn segments(row: &Value) -> &[Value] {
        assert_eq!(row["ok"], true);
        row["segments"].as_array().unwrap()
    }
    /// What every converted row promises about its own parts.
    fn assert_consistent(row: &Value, limit: usize) {
        let mut reading = String::new();
        let mut top1 = String::new();
        for segment in segments(row) {
            let shown = values(segment);
            assert!(!shown.is_empty() && shown.len() <= limit);
            assert!(segment["total"].as_u64().unwrap() >= shown.len() as u64);
            assert_eq!(shown.iter().collect::<HashSet<_>>().len(), shown.len());
            reading.push_str(segment["reading"].as_str().unwrap());
            top1.push_str(shown[0]);
        }
        assert_eq!(row["reading"], reading);
        assert_eq!(row["top1"], top1);
    }

    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn every_segment_is_exported_with_its_own_candidates() {
        // No part of this reading has candidates made from the date or time.
        let reading = "わたしはがっこうへいきます";
        let input = line("1", reading, "") + &line("2", "ワタシハガッコウヘイキマス", "");
        let (header, rows) = export(&["--limit", "12"], &input);
        assert_eq!(header["schema"], SCHEMA);
        assert_eq!(header["limit"], 12);
        assert_eq!(header["privacy"], "private");
        assert_eq!(header["profile"], "fresh");
        assert_eq!(header["worker"]["sha256"].as_str().unwrap().len(), 64);
        assert!(header["worker"]["protocol"].is_u64());
        assert!(header["mozc"]["tag"].is_string() && header["mozc"]["commit"].is_string());
        assert!(header["mozc"]["data_version"].is_u64());
        assert!(header["mozc"]["engine_version"].is_u64());
        assert!(header["kanade_commit"].is_string() || header["kanade_commit"].is_null());

        assert_eq!(rows[0]["id"], "1");
        assert_eq!(rows[0]["reading"], reading);
        assert_consistent(&rows[0], 12);
        let exported = segments(&rows[0]);
        assert!(exported.len() >= 2);
        // A later segment lists its own candidates, not the first segment's.
        assert!(values(&exported[0]).contains(&"私は"));
        assert!(values(&exported[1]).contains(&"学校へ"));
        assert!(!values(&exported[1]).contains(&"私は"));
        assert!(
            exported
                .iter()
                .flat_map(|segment| segment["candidates"].as_array().unwrap())
                .any(|candidate| candidate["transliteration"] == true)
        );
        // Katakana is the same request once it is hiragana.
        assert_eq!(rows[1]["reading"], reading);
        assert_eq!(rows[1]["segments"], rows[0]["segments"]);

        // The limit cuts the list after duplicates are removed, not the count.
        let (_, cut) = export(&["--limit", "3"], &input);
        assert_consistent(&cut[0], 3);
        for (short, long) in segments(&cut[0]).iter().zip(exported) {
            assert_eq!(short["total"], long["total"]);
            assert_eq!(values(short), values(long)[..3]);
        }
    }

    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn suggestions_change_the_typing_and_not_the_response() {
        // While composing, the worker answers a key with suggestions only
        // when it is asked for them; the composition is the same either way.
        let compose = |suggest: bool| {
            let options = Options {
                suggest,
                ..Options::default()
            };
            let mut backend =
                Backend::start(&kanade_mozc::default_worker(), None, &options).unwrap();
            let mozc = &mut backend.mozc;
            let session = mozc.create_session().unwrap();
            let mut typed = Snapshot::default();
            for c in "nihongo".chars() {
                typed = mozc.key(session, &key(c), "").unwrap();
            }
            (kanade_ipc::reading(&typed.preedit), typed.candidates.len())
        };
        let (suggested, suggestions) = compose(true);
        assert!(suggestions > 0);
        assert_eq!(compose(false), (suggested, 0));

        let input = line("1", "わたしはがっこうへいきます", "")
            + &line(
                "2",
                "でんしゃにのって、こうこくにのっているないようをよむ。",
                "",
            )
            + &line("3", "こ", "りんごを10")
            + &line(
                "4",
                "ふぁいるをほぞんしてから、めーるをおくりました",
                "それから、",
            )
            + &line("5", "にほんご", "");
        let (_, quiet) = export(&[], &input);
        let (_, suggesting) = export(&["--suggest"], &input);
        assert_eq!(quiet.len(), 5);
        assert!(quiet.iter().all(|row| row["ok"] == true));
        assert_eq!(quiet, suggesting);
    }

    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn punctuation_stays_in_the_reading() {
        let reading = "でんしゃにのって、こうこくにのっているないようをよむ。";
        let (_, rows) = export(&[], line("1", reading, ""));
        assert_eq!(rows[0]["reading"], reading);
        assert_consistent(&rows[0], 50);
        // The locked worker gives each mark a segment of its own.
        for mark in ["、", "。"] {
            let segment = segments(&rows[0])
                .iter()
                .find(|segment| segment["reading"] == mark)
                .unwrap();
            assert_eq!(values(segment)[0], mark);
        }
        // Other punctuation is not typed for the caller.
        let (_, rows) = export(&[], line("2", "でんしゃにのって，よむ．", ""));
        assert_eq!(rows[0]["error"], "unsupported_reading");
    }

    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn longer_readings_need_max_reading() {
        let reading = "でんしゃにのって、こうこくにのっているないようをよむ。".repeat(9);
        let input = line("1", &reading, "");
        let (_, rows) = export(&[], &input);
        assert_eq!(rows[0]["error"], "unsupported_reading");
        // Conversions this long are why the export outlasts the key deadline.
        let (_, rows) = export(&["--max-reading", "400"], &input);
        assert_eq!(rows[0]["reading"], reading.as_str());
        assert_consistent(&rows[0], 50);
        // The worker stops composing first; that is reported, never cut short.
        let beyond = line("2", &"あ".repeat(300), "");
        let (_, rows) = export(&["--max-reading", "400"], &beyond);
        assert_eq!(rows[0]["error"], "reading_mismatch");
    }

    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn responses_do_not_depend_on_order_or_neighbors() {
        let requests = [
            ("わたしはがっこうへいきます", ""),
            ("かれがてがみをかきました", ""),
            ("こ", "りんごを10"),
            ("こ", ""),
            ("にわにはにわにわとりがいる", ""),
            ("ふぁいるをほぞんする", "あたらしい"),
            ("こうえんでいぬとあそぶ", ""),
            ("しんかんせんでおおさかへ、いく。", ""),
            ("らーめんをたべたい", ""),
            ("こ", "りんごを10"),
            ("わたしはがっこうへいきます", ""),
            ("ぷろぐらむをかく", ""),
        ];
        let lines: Vec<_> = requests
            .iter()
            .enumerate()
            .map(|(index, (reading, preceding))| line(&format!("r{index}"), reading, preceding))
            .collect();
        let run_in = |order: &[usize]| {
            let input: String = order.iter().map(|index| lines[*index].as_str()).collect();
            let (_, rows) = export(&[], &input);
            assert_eq!(rows.len(), order.len());
            let mut by_id = vec![Value::Null; lines.len()];
            for (index, row) in order.iter().zip(rows) {
                assert_eq!(row["id"], format!("r{index}"));
                assert_consistent(&row, 50);
                by_id[*index] = row;
            }
            by_id
        };
        let forward: Vec<_> = (0..lines.len()).collect();
        let reverse: Vec<_> = forward.iter().rev().copied().collect();
        let shuffled: Vec<_> = (0..lines.len())
            .map(|i| (i * 5 + 3) % lines.len())
            .collect();
        let first = run_in(&forward);
        assert_eq!(run_in(&reverse), first);
        assert_eq!(run_in(&shuffled), first);
        // The context changes a response, but only its own.
        assert_ne!(first[2]["top1"], first[3]["top1"]);
        // The same request has one answer wherever it appears in a run.
        for (earlier, later) in [(2, 9), (0, 10)] {
            assert_eq!(first[earlier]["segments"], first[later]["segments"]);
            assert_eq!(first[earlier]["top1"], first[later]["top1"]);
        }
    }

    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn every_line_is_answered_in_place() {
        let longest = "あ".repeat(64);
        let lines = [
            request("a", "がっこうへいく", "").into_bytes(),
            b"not json".to_vec(),
            request("b", "漢字", "").into_bytes(),
            request("c", "", "").into_bytes(),
            Vec::new(),
            r#"{"id":"d","reading":"カタカナ"}"#.as_bytes().to_vec(),
            r#"{"id":"e","reading":12,"preceding":""}"#.as_bytes().to_vec(),
            r#"{"id":13,"reading":"あ","preceding":""}"#.as_bytes().to_vec(),
            r#"{"id":"f","reading":"あ","preceding":"","extra":1}"#
                .as_bytes()
                .to_vec(),
            request("g", &format!("{longest}あ"), "").into_bytes(),
            request("h", &longest, "").into_bytes(),
            request("i", "あ　い", "").into_bytes(),
            // Not text at all, then a line that ends with a carriage return.
            b"{\"id\":\"k\",\"reading\":\"\xff\"}".to_vec(),
            (request("l", "あ", "") + "\r").into_bytes(),
            // The last line has no line ending.
            request("j", "やっぱりいく", "").into_bytes(),
        ];
        let (_, rows) = export(&[], lines.join(&b'\n'));
        let outcomes: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row["id"].as_str(),
                    row["ok"].as_bool(),
                    row["error"].as_str(),
                )
            })
            .collect();
        let ok = |id| (Some(id), Some(true), None);
        let failed = |id, error| (id, Some(false), Some(error));
        assert_eq!(
            outcomes,
            [
                ok("a"),
                failed(None, "bad_request"),
                failed(Some("b"), "unsupported_reading"),
                failed(Some("c"), "unsupported_reading"),
                failed(None, "bad_request"),
                ok("d"),
                failed(Some("e"), "bad_request"),
                failed(None, "bad_request"),
                failed(Some("f"), "bad_request"),
                failed(Some("g"), "unsupported_reading"),
                ok("h"),
                failed(Some("i"), "unsupported_reading"),
                failed(None, "bad_request"),
                ok("l"),
                ok("j"),
            ]
        );
        // A failed row has no other fields to mistake for a result.
        assert_eq!(
            rows[1],
            json!({"id": null, "ok": false, "error": "bad_request"})
        );
    }

    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn a_failed_worker_does_not_cost_its_row() {
        let mut exporter = Exporter::start(Options::default()).unwrap();
        let request = request("1", "わたしはがっこうへいきます", "");
        let first = exporter.respond(1, request.as_bytes());
        assert!(matches!(first, Response::Converted { .. }));
        // The row that meets a dead worker is answered by its replacement:
        // the same artifact on a new profile, so the answer is the same.
        exporter.backend.as_mut().unwrap().mozc.terminate().unwrap();
        assert_eq!(exporter.respond(2, request.as_bytes()), first);
        let replacement = exporter.backend.as_ref().unwrap();
        assert_eq!(replacement.mozc.identity(), &exporter.identity);
        assert_eq!(exporter.respond(3, request.as_bytes()), first);
    }

    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn a_row_that_fails_twice_is_reported_and_the_next_one_answered() {
        let mut exporter = Exporter::start(options(&["--max-reading", "400"]).unwrap()).unwrap();
        // No worker converts a reading this long within one millisecond, so
        // the first attempt and the retry both run out of time.
        let long = request("1", &"でんしゃにのって、こうこくにのる。".repeat(12), "");
        exporter.backend = None;
        exporter.options.deadline_ms = 1;
        assert_eq!(
            exporter.respond(1, long.as_bytes()),
            Response::failed(Some("1".into()), Fault::Engine)
        );
        assert!(exporter.backend.is_none());
        exporter.options.deadline_ms = Options::default().deadline_ms;
        let next = exporter.respond(2, long.as_bytes());
        assert!(matches!(next, Response::Converted { .. }));
    }

    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn table_spells_what_the_worker_composes() {
        let options = Options::default();
        let mut backend = Backend::start(&kanade_mozc::default_worker(), None, &options).unwrap();
        let mozc = &mut backend.mozc;
        let session = mozc.create_session().unwrap();
        let mut compose = |reading: &str| {
            let mut typed = Snapshot::default();
            for c in romaji::keys(reading).unwrap().chars() {
                typed = mozc.key(session, &key(c), "").unwrap();
                assert!(typed.consumed && typed.commit.is_none());
            }
            mozc.reset(session).unwrap();
            assert_eq!(kanade_ipc::reading(&typed.preedit), reading);
        };
        let mut units = 0;
        for (kana, _) in romaji::syllables() {
            units += 1;
            // Alone, after and before the sounds whose keys depend on a neighbor.
            for reading in [
                kana.to_owned(),
                format!("っ{kana}"),
                format!("っっ{kana}"),
                format!("ん{kana}"),
                format!("{kana}ん"),
                format!("{kana}っ"),
                format!("{kana}ー{kana}"),
                format!("あ{kana}{kana}を"),
            ] {
                compose(&reading);
            }
        }
        assert!(units > 200);
        compose("っ");
        compose("んっんー");
    }
}
