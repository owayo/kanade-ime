// SPDX-License-Identifier: MIT
//! On-disk records: the imported model, the consent given for it and the
//! qualification measured with it. Records hold identities and counts only.
use crate::{
    profile::{self, Model},
    qualify::Measurement,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Install records are unchanged since the first version, so a stored
/// consent survives the introduction of profiles.
const SCHEMA: u32 = 1;
/// Version 2 binds a measurement to the model's profile. Version 1 had no
/// such binding and never makes a model ready.
const QUALIFICATION_SCHEMA: u32 = 2;
const MODEL: &str = "model.gguf";
const INSTALL: &str = "install.json";
const QUALIFICATION: &str = "qualification.json";

/// Cheap identity of a file, compared before any expensive hashing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub size: u64,
    pub modified_ns: u64,
}
impl Stamp {
    pub fn of(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(Self {
            size: meta.len(),
            modified_ns: meta
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    pub schema: u32,
    pub model: String,
    pub sha256: String,
    pub size: u64,
    pub license: String,
    /// Digest of the terms the user accepted.
    pub terms_sha256: String,
    pub accepted_at: u64,
    pub stamp: Stamp,
}
impl Install {
    /// Whether this record still describes `model`'s file.
    pub fn names(&self, model: &Model) -> bool {
        self.model == model.id && self.sha256 == model.sha256 && self.size == model.size
    }
    /// Whether the stored consent is a consent to the current terms of
    /// `model`: same licence and the same wording, including what is done
    /// with the user's text.
    pub fn consents(&self, model: &Model) -> bool {
        self.license == model.license && self.terms_sha256 == profile::terms_sha256(model)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub schema: u32,
    pub model: String,
    pub model_sha256: String,
    pub model_size: u64,
    pub vocab_sha256: String,
    /// The profile the measurement was made with, and the digest of all of it.
    pub profile: String,
    pub profile_version: u32,
    pub profile_sha256: String,
    pub worker_sha256: String,
    pub runtime: String,
    /// Parts of the profile, kept separately so that a refusal names its cause.
    pub prompt_sha256: String,
    pub tokenizer_sha256: String,
    pub parity_sha256: String,
    pub ranking_sha256: String,
    pub measurement: Measurement,
    pub created_at: u64,
}
/// A measured result bound to every identity it depends on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Qualification {
    pub evidence: Evidence,
    /// Digest of `evidence`; detects truncated or hand-edited records.
    pub digest: String,
}
/// What the qualification file holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    /// No file, or one that is not a qualification record.
    Missing,
    /// A record written before profiles existed. It is bound to no profile.
    Unbound,
    Bound(Box<Qualification>),
}
impl Recorded {
    pub fn bound(&self) -> Option<&Qualification> {
        match self {
            Self::Bound(qualification) => Some(qualification),
            _ => None,
        }
    }
}
/// Shape of a version-1 record. Read only to tell "measured before profiles
/// existed" from "no record"; none of its values is used.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct LegacyEvidence {
    schema: u32,
    model: String,
    model_sha256: String,
    model_size: u64,
    vocab_sha256: String,
    worker_sha256: String,
    runtime: String,
    prompt_sha256: String,
    parity_sha256: String,
    ranking_sha256: String,
    measurement: Measurement,
    created_at: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct LegacyQualification {
    evidence: LegacyEvidence,
    digest: String,
}

/// Why an installed model is not usable. Fixed values, no free text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    None,
    WorkerMissing,
    ModelMissing,
    ModelChanged,
    RuntimeChanged,
    PromptChanged,
    ReportMissing,
    ReportInvalid,
    QualityGate,
    TokenizerMismatch,
    WorkerFailed,
    /// The qualification is not bound to the profile this build registers for
    /// the model. Qualifying again resolves it.
    ProfileChanged,
    /// The installed model is not registered in this build.
    ModelUnsupported,
    /// The stored consent is not a consent to the current terms.
    ConsentChanged,
}

impl Evidence {
    pub fn new(model: &Model, worker_sha256: String, measurement: Measurement) -> Self {
        let profile = model.profile;
        Self {
            schema: QUALIFICATION_SCHEMA,
            model: model.id.into(),
            model_sha256: model.sha256.into(),
            model_size: model.size,
            vocab_sha256: profile.vocab_sha256.into(),
            profile: profile.id.into(),
            profile_version: profile.version,
            profile_sha256: profile.sha256(),
            worker_sha256,
            runtime: profile::RUNTIME.into(),
            prompt_sha256: profile.prompt.sha256(),
            tokenizer_sha256: profile.tokenizer.sha256(),
            parity_sha256: profile.parity_sha256(),
            ranking_sha256: profile.ranking_sha256(),
            measurement,
            created_at: now(),
        }
    }
    fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        profile::hex(&Sha256::digest(bytes))
    }
    pub fn seal(self) -> Qualification {
        Qualification {
            digest: self.digest(),
            evidence: self,
        }
    }
}
impl Qualification {
    /// Re-derives readiness from the recorded identities and counts. No stored
    /// flag is trusted: every value is compared with what this build registers
    /// for the installed model and with the files that are present now. The
    /// profile is always the one registered for `install.model`; a record
    /// cannot choose another.
    pub fn verdict(&self, install: &Install, worker_sha256: &str) -> Reason {
        let e = &self.evidence;
        if e.schema != QUALIFICATION_SCHEMA || self.digest != e.digest() {
            return Reason::ReportInvalid;
        }
        let Some(model) = profile::model(&install.model) else {
            return Reason::ModelUnsupported;
        };
        let profile = model.profile;
        if !install.names(model)
            || e.model != model.id
            || e.model_sha256 != model.sha256
            || e.model_size != model.size
            || e.vocab_sha256 != profile.vocab_sha256
        {
            return Reason::ModelChanged;
        }
        if e.runtime != profile::RUNTIME || e.worker_sha256 != worker_sha256 {
            return Reason::RuntimeChanged;
        }
        if e.profile != profile.id || e.profile_version != profile.version {
            return Reason::ProfileChanged;
        }
        if e.prompt_sha256 != profile.prompt.sha256() {
            return Reason::PromptChanged;
        }
        if e.tokenizer_sha256 != profile.tokenizer.sha256()
            || e.parity_sha256 != profile.parity_sha256()
        {
            return Reason::TokenizerMismatch;
        }
        // Ranking corpus, gates, start and end token, bounds fixture.
        if e.ranking_sha256 != profile.ranking_sha256() || e.profile_sha256 != profile.sha256() {
            return Reason::ProfileChanged;
        }
        e.measurement.verdict(profile)
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// SHA-256 of a file, read in bounded chunks. Management thread only.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let len = file.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        digest.update(&buffer[..len]);
    }
    Ok(profile::hex(&digest.finalize()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportError {
    /// The source is missing, unreadable or not a regular file.
    Source,
    /// Size or digest differs from the pinned model.
    Mismatch,
    Storage,
}

pub struct Store {
    dir: PathBuf,
}
impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
    pub fn model_path(&self) -> PathBuf {
        self.dir.join(MODEL)
    }
    pub fn install(&self) -> Option<Install> {
        let install: Install = read_json(&self.dir.join(INSTALL))?;
        (install.schema == SCHEMA && profile::is_sha256(&install.sha256)).then_some(install)
    }
    pub fn qualification(&self) -> Recorded {
        let path = self.dir.join(QUALIFICATION);
        let Some(bytes) = read_record(&path) else {
            return Recorded::Missing;
        };
        if let Ok(qualification) = serde_json::from_slice::<Qualification>(&bytes) {
            return Recorded::Bound(Box::new(qualification));
        }
        match serde_json::from_slice::<LegacyQualification>(&bytes) {
            Ok(legacy) if legacy.evidence.schema == 1 => Recorded::Unbound,
            _ => Recorded::Missing,
        }
    }
    /// Whether the model file still looks like the one that was imported.
    pub fn model_reason(&self, install: &Install) -> Reason {
        match Stamp::of(&self.model_path()) {
            Ok(stamp) if stamp == install.stamp => Reason::None,
            Ok(_) => Reason::ModelChanged,
            Err(_) => Reason::ModelMissing,
        }
    }
    /// Copies and verifies a user-supplied file. The copy stops as soon as it
    /// exceeds the pinned size, and a wrong file never replaces a good one.
    pub fn import(
        &self,
        source: &Path,
        model: &Model,
        mut progress: impl FnMut(u64),
    ) -> Result<Install, ImportError> {
        let mut input = File::open(source).map_err(|_| ImportError::Source)?;
        let meta = input.metadata().map_err(|_| ImportError::Source)?;
        if !meta.is_file() {
            return Err(ImportError::Source);
        }
        if meta.len() != model.size {
            return Err(ImportError::Mismatch);
        }
        self.prepare().map_err(|_| ImportError::Storage)?;
        let staging = self.dir.join("model.import");
        let result = (|| {
            let mut output = private_file(&staging).map_err(|_| ImportError::Storage)?;
            let mut digest = Sha256::new();
            let mut buffer = vec![0u8; 1 << 20];
            let mut copied = 0u64;
            loop {
                let len = input.read(&mut buffer).map_err(|_| ImportError::Source)?;
                if len == 0 {
                    break;
                }
                copied += len as u64;
                if copied > model.size {
                    return Err(ImportError::Mismatch);
                }
                digest.update(&buffer[..len]);
                output
                    .write_all(&buffer[..len])
                    .map_err(|_| ImportError::Storage)?;
                progress(copied);
            }
            if copied != model.size || profile::hex(&digest.finalize()) != model.sha256 {
                return Err(ImportError::Mismatch);
            }
            output.sync_all().map_err(|_| ImportError::Storage)?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }
        // A previous qualification describes another file state.
        let _ = fs::remove_file(self.dir.join(QUALIFICATION));
        let _ = fs::remove_file(self.dir.join(INSTALL));
        let target = self.model_path();
        read_only(&staging, false);
        let _ = fs::remove_file(&target);
        fs::rename(&staging, &target).map_err(|_| ImportError::Storage)?;
        read_only(&target, true);
        Ok(Install {
            schema: SCHEMA,
            model: model.id.into(),
            sha256: model.sha256.into(),
            size: model.size,
            license: model.license.into(),
            terms_sha256: profile::terms_sha256(model),
            accepted_at: now(),
            stamp: Stamp::of(&target).map_err(|_| ImportError::Storage)?,
        })
    }
    pub fn save_install(&self, install: &Install) -> io::Result<()> {
        self.prepare()?;
        write_json(&self.dir.join(INSTALL), install)
    }
    pub fn save_qualification(&self, qualification: &Qualification) -> io::Result<()> {
        self.prepare()?;
        write_json(&self.dir.join(QUALIFICATION), qualification)
    }
    pub fn clear_qualification(&self) {
        let _ = fs::remove_file(self.dir.join(QUALIFICATION));
    }
    /// Deletes the model and every record. Missing files are not an error.
    pub fn remove(&self) -> io::Result<()> {
        let model = self.model_path();
        read_only(&model, false);
        for path in [
            self.dir.join(QUALIFICATION),
            self.dir.join(INSTALL),
            model,
            self.dir.join("model.import"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn prepare(&self) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

fn private_file(path: &Path) -> io::Result<File> {
    let mut options = File::options();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn read_only(path: &Path, value: bool) {
    if let Ok(meta) = fs::metadata(path) {
        let mut permissions = meta.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(if value { 0o400 } else { 0o600 });
        }
        #[cfg(not(unix))]
        permissions.set_readonly(value);
        let _ = fs::set_permissions(path, permissions);
    }
}
fn read_record(path: &Path) -> Option<Vec<u8>> {
    let meta = fs::metadata(path).ok()?;
    // Records are tiny; refuse anything else before reading it.
    if !meta.is_file() || meta.len() > 64 * 1024 {
        return None;
    }
    fs::read(path).ok()
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&read_record(path)?).ok()
}
fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let staging = path.with_extension("next");
    let mut file = private_file(&staging)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    fs::rename(staging, path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        profile::{CATALOG, fixture},
        qualify,
    };

    const WORKER: &str = "77777777777777777777777777777777777777777777777777777777777777aa";
    fn install(model: &Model) -> Install {
        Install {
            schema: SCHEMA,
            model: model.id.into(),
            sha256: model.sha256.into(),
            size: model.size,
            license: model.license.into(),
            terms_sha256: profile::terms_sha256(model),
            accepted_at: 1,
            stamp: Stamp {
                size: model.size,
                modified_ns: 1,
            },
        }
    }
    fn passing(model: &Model) -> Measurement {
        let profile = model.profile;
        Measurement {
            tokenizer_cases: qualify::parity_count(profile),
            tokenizer_mismatches: 0,
            ranking_cases: qualify::ranking_count(profile),
            ranking_top1: qualify::ranking_count(profile),
            ranking_unscored: 0,
            unstable: 0,
            bound_failures: 0,
            p50_ms: 20,
            p95_ms: 40,
            max_ms: 60,
        }
    }
    fn evidence(model: &Model) -> Evidence {
        Evidence::new(model, WORKER.into(), passing(model))
    }
    /// A record exactly as the version before profiles wrote it.
    pub(crate) fn legacy_qualification(model: &Model) -> String {
        let profile = model.profile;
        let evidence = serde_json::json!({
            "schema": 1,
            "model": model.id,
            "model_sha256": model.sha256,
            "model_size": model.size,
            "vocab_sha256": profile.vocab_sha256,
            "worker_sha256": WORKER,
            "runtime": "llama-cpp-sys-2 0.1.160 llama.cpp d8123504 cpu pre=gpt-2",
            "prompt_sha256": "1".repeat(64),
            "parity_sha256": profile.parity_sha256(),
            "ranking_sha256": profile.ranking_sha256(),
            "measurement": passing(model),
            "created_at": 1_700_000_000u64,
        });
        let digest = profile::hex(&Sha256::digest(serde_json::to_vec(&evidence).unwrap()));
        serde_json::to_string_pretty(&serde_json::json!({"evidence": evidence, "digest": digest}))
            .unwrap()
    }
    /// An install record exactly as the version before profiles wrote it.
    pub(crate) fn legacy_install(model: &Model, stamp: Stamp) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": 1,
            "model": model.id,
            "sha256": model.sha256,
            "size": model.size,
            "license": model.license,
            "terms_sha256": crate::profile::terms_sha256(model),
            "accepted_at": 1_700_000_000u64,
            "stamp": {"size": stamp.size, "modified_ns": stamp.modified_ns},
        }))
        .unwrap()
    }

    #[test]
    fn a_record_is_ready_only_for_the_model_and_profile_it_was_measured_with() {
        let tsumugi = &CATALOG[0];
        let alt = fixture::alt();
        for model in [tsumugi, alt] {
            let record = evidence(model).seal();
            assert_eq!(record.verdict(&install(model), WORKER), Reason::None);
            assert_eq!(record.evidence.profile, model.profile.id);
            assert_eq!(record.evidence.profile_sha256, model.profile.sha256());
        }
        // The two models are the same file. A record measured under one
        // profile is refused for the other model, whichever way round.
        assert_eq!(
            evidence(alt).seal().verdict(&install(tsumugi), WORKER),
            Reason::ModelChanged
        );
        assert_eq!(
            evidence(tsumugi).seal().verdict(&install(alt), WORKER),
            Reason::ModelChanged
        );
        // A record that names the installed model but another profile: the
        // wrong combination, however consistent the rest of it is.
        let mut wrong = evidence(alt);
        wrong.model = tsumugi.id.into();
        assert_eq!(
            wrong.seal().verdict(&install(tsumugi), WORKER),
            Reason::ProfileChanged
        );
        let mut wrong = evidence(tsumugi);
        wrong.profile = alt.profile.id.into();
        wrong.profile_version = alt.profile.version;
        wrong.profile_sha256 = alt.profile.sha256();
        assert_eq!(
            wrong.seal().verdict(&install(tsumugi), WORKER),
            Reason::ProfileChanged
        );
        // An install record for a model this build does not register.
        let mut unknown = install(tsumugi);
        unknown.model = "unregistered".into();
        assert_eq!(
            evidence(tsumugi).seal().verdict(&unknown, WORKER),
            Reason::ModelUnsupported
        );
    }
    #[test]
    fn every_bound_identity_invalidates_the_record_when_it_changes() {
        let tsumugi = &CATALOG[0];
        type Edit = fn(&mut Evidence);
        let cases: [(Edit, Reason); 16] = [
            (|e| e.schema = 1, Reason::ReportInvalid),
            (|e| e.model = "other".into(), Reason::ModelChanged),
            (|e| e.model_sha256 = "1".repeat(64), Reason::ModelChanged),
            (|e| e.model_size += 1, Reason::ModelChanged),
            (|e| e.vocab_sha256 = "1".repeat(64), Reason::ModelChanged),
            (
                |e| e.runtime = "another runtime".into(),
                Reason::RuntimeChanged,
            ),
            (|e| e.worker_sha256 = "1".repeat(64), Reason::RuntimeChanged),
            (|e| e.profile = "tsumugi".into(), Reason::ProfileChanged),
            (|e| e.profile_version += 1, Reason::ProfileChanged),
            (|e| e.prompt_sha256 = "1".repeat(64), Reason::PromptChanged),
            (
                |e| e.tokenizer_sha256 = "1".repeat(64),
                Reason::TokenizerMismatch,
            ),
            (
                |e| e.parity_sha256 = "1".repeat(64),
                Reason::TokenizerMismatch,
            ),
            (
                |e| e.ranking_sha256 = "1".repeat(64),
                Reason::ProfileChanged,
            ),
            // Gates, start and end token are covered by the profile digest.
            (
                |e| e.profile_sha256 = "1".repeat(64),
                Reason::ProfileChanged,
            ),
            (
                |e| e.measurement.tokenizer_mismatches = 1,
                Reason::TokenizerMismatch,
            ),
            (|e| e.measurement.ranking_top1 = 10, Reason::QualityGate),
        ];
        for (index, (edit, reason)) in cases.into_iter().enumerate() {
            let mut changed = evidence(tsumugi);
            edit(&mut changed);
            assert_eq!(
                changed.seal().verdict(&install(tsumugi), WORKER),
                reason,
                "case {index}"
            );
        }
        // An edit after sealing fails the record digest, whatever it claims.
        let mut edited = evidence(tsumugi).seal();
        edited.evidence.measurement.ranking_top1 -= 1;
        assert_eq!(
            edited.verdict(&install(tsumugi), WORKER),
            Reason::ReportInvalid
        );
        // An install record that no longer describes the registered file.
        let mut other_file = install(tsumugi);
        other_file.sha256 = "1".repeat(64);
        assert_eq!(
            evidence(tsumugi).seal().verdict(&other_file, WORKER),
            Reason::ModelChanged
        );
    }
    #[test]
    fn records_written_before_profiles_keep_the_consent_and_lose_readiness() {
        let tsumugi = &CATALOG[0];
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().into());
        assert_eq!(store.qualification(), Recorded::Missing);
        assert_eq!(store.install(), None);

        let stamp = Stamp {
            size: tsumugi.size,
            modified_ns: 5,
        };
        fs::write(dir.path().join(INSTALL), legacy_install(tsumugi, stamp)).unwrap();
        fs::write(
            dir.path().join(QUALIFICATION),
            legacy_qualification(tsumugi),
        )
        .unwrap();
        // The install record is read as it is, and its consent still names
        // the current terms and licence.
        let install = store.install().unwrap();
        assert!(install.names(tsumugi) && install.consents(tsumugi));
        assert_eq!(install.accepted_at, 1_700_000_000);
        // The old measurement is recognised and bound to nothing.
        assert_eq!(store.qualification(), Recorded::Unbound);
        assert!(store.qualification().bound().is_none());

        // A current record round-trips.
        let current = evidence(tsumugi).seal();
        store.save_qualification(&current).unwrap();
        assert_eq!(
            store.qualification(),
            Recorded::Bound(Box::new(current.clone()))
        );
        // An old-format record that declares the new schema, a current record
        // with a claimed pass flag, a truncated one and other JSON are no
        // records at all.
        let path = dir.path().join(QUALIFICATION);
        let legacy: serde_json::Value =
            serde_json::from_str(&legacy_qualification(tsumugi)).unwrap();
        let mut upgraded = legacy.clone();
        upgraded["evidence"]["schema"] = 2.into();
        let mut flagged = serde_json::to_value(&current).unwrap();
        flagged["evidence"]["passed"] = true.into();
        let mut flagged_legacy = legacy.clone();
        flagged_legacy["ready"] = true.into();
        let text = serde_json::to_string(&current).unwrap();
        for bytes in [
            serde_json::to_vec(&upgraded).unwrap(),
            serde_json::to_vec(&flagged).unwrap(),
            serde_json::to_vec(&flagged_legacy).unwrap(),
            text.as_bytes()[..text.len() / 2].to_vec(),
            b"{\"ready\":true}".to_vec(),
            b"[]".to_vec(),
            Vec::new(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(store.qualification(), Recorded::Missing);
        }
    }
    #[test]
    fn a_consent_covers_only_the_terms_it_was_given_for() {
        let tsumugi = &CATALOG[0];
        let record = install(tsumugi);
        assert!(record.names(tsumugi) && record.consents(tsumugi));
        let mut other_terms = install(tsumugi);
        other_terms.terms_sha256 = "1".repeat(64);
        assert!(!other_terms.consents(tsumugi));
        let mut other_licence = install(tsumugi);
        other_licence.license = "MIT".into();
        assert!(!other_licence.consents(tsumugi));
        // The same file under another registration has its own terms.
        assert!(!record.names(fixture::alt()) && !record.consents(fixture::alt()));
    }
}
