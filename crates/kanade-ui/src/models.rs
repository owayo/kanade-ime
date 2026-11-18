//! Explicit local model controls. Importing a model requires both consents.
use anyhow::{Result, bail, ensure};
use kanade_ipc::{Manage, manage::Action};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub license: String,
    pub license_url: String,
    pub source_url: String,
    pub attribution: String,
    pub terms_sha256: String,
    pub profile: Option<String>,
    pub profile_sha256: Option<String>,
}
#[derive(Clone, Deserialize)]
pub struct Job {
    pub phase: String,
    pub error: String,
}
#[derive(Clone, Deserialize)]
pub struct InstalledModel {
    pub id: String,
}
#[derive(Clone, Deserialize)]
pub struct Status {
    pub ready: bool,
    pub enabled: bool,
    pub runtime: String,
    pub state: String,
    pub reason: String,
    pub catalog: Vec<CatalogEntry>,
    pub installed: Option<InstalledModel>,
    pub job: Option<Job>,
}

/// Consent belongs to the displayed model and exact terms, never a list index.
#[derive(Default)]
pub struct ModelSelection {
    key: Option<SelectionKey>,
    pub license: bool,
    pub local_processing: bool,
}
#[derive(PartialEq, Eq)]
struct SelectionKey {
    id: String,
    terms: String,
    profile: Option<String>,
    profile_sha256: Option<String>,
}
impl SelectionKey {
    fn of(model: &CatalogEntry) -> Self {
        Self {
            id: model.id.clone(),
            terms: model.terms_sha256.clone(),
            profile: model.profile.clone(),
            profile_sha256: model.profile_sha256.clone(),
        }
    }
}
impl ModelSelection {
    pub fn refresh(&mut self, status: Option<&Status>) {
        let model = status.and_then(|status| {
            self.key
                .as_ref()
                .and_then(|key| status.catalog.iter().find(|model| model.id == key.id))
                .or_else(|| {
                    status.installed.as_ref().and_then(|installed| {
                        status.catalog.iter().find(|model| model.id == installed.id)
                    })
                })
                .or_else(|| status.catalog.first())
        });
        self.select(model);
    }
    pub fn select(&mut self, model: Option<&CatalogEntry>) {
        let key = model.map(SelectionKey::of);
        if self.key != key {
            self.key = key;
            self.license = false;
            self.local_processing = false;
        }
    }
    pub fn selected<'a>(&self, status: &'a Status) -> Option<&'a CatalogEntry> {
        status.catalog.iter().find(|model| self.matches(model))
    }
    pub fn matches(&self, model: &CatalogEntry) -> bool {
        self.key.as_ref() == Some(&SelectionKey::of(model))
    }
    pub fn accepted(&self, model: &CatalogEntry) -> bool {
        self.matches(model) && self.license && self.local_processing
    }
}
impl Status {
    pub fn busy(&self) -> bool {
        self.state == "loading"
            || self
                .job
                .as_ref()
                .is_some_and(|job| !matches!(job.phase.as_str(), "done" | "failed"))
    }
    pub fn message(&self) -> &'static str {
        if self.state == "loading" {
            return "モデルの導入状態を確認しています。";
        }
        if self.runtime == "missing" {
            return "推論プログラムが見つかりません。Kanadeを更新してください。";
        }
        if let Some(job) = &self.job {
            match job.phase.as_str() {
                "queued" => return "モデルの処理を準備しています。",
                "copying" => return "モデルを取り込んでいます。",
                "hashing" => return "モデルの内容を確認しています。",
                "starting_runtime" => return "推論プログラムを起動しています。",
                "tokenizer" => return "モデルの文字処理を検証しています。",
                "bounds" => return "モデルの処理量の上限を検証しています。",
                "ranking" | "timing" => return "候補の評価結果と応答時間を検証しています。",
                "failed" => {
                    return match job.error.as_str() {
                        "model_mismatch" => {
                            "対応するモデルと内容が一致しません。配布元とファイルを確認してください。"
                        }
                        "tokenizer_mismatch" => {
                            "モデルの文字処理が一致しません。このモデルは有効にできません。"
                        }
                        "quality_gate" => {
                            "候補の評価または応答時間が基準を満たしません。このモデルは有効にできません。"
                        }
                        "consent_required" => {
                            "利用条件が更新されています。内容を確認し、同意して取り込み直してください。"
                        }
                        _ => "モデルの処理に失敗しました。状態を更新して確認してください。",
                    };
                }
                _ => {}
            }
        }
        if self.ready {
            return if self.enabled {
                "検証済みのモデルで、変換候補を文脈に合わせて並べ替えます。"
            } else {
                "モデルの検証が完了しました。候補の並べ替えを有効にできます。"
            };
        }
        match self.state.as_str() {
            "loading" => "モデルの導入状態を確認しています。",
            "not_installed" => {
                "モデルは未導入です。配布元から取得したGGUFファイルを取り込んでください。"
            }
            "qualifying" => "モデルを検証しています。通常の変換はそのまま使えます。",
            _ if self.reason == "model_unsupported" => {
                "導入済みのモデルはこのバージョンで利用できません。対応モデルを取り込み直してください。"
            }
            _ if self.reason == "consent_changed" => {
                "利用条件が更新されています。内容を確認し、同意して取り込み直してください。"
            }
            _ if self.reason == "model_changed" => {
                "モデルが変更されています。対応するファイルを導入し直してください。"
            }
            _ if matches!(
                self.reason.as_str(),
                "runtime_changed" | "prompt_changed" | "profile_changed"
            ) =>
            {
                "推論プログラムまたはモデルの設定が更新されています。モデルを再検証してください。"
            }
            _ if self.reason == "worker_failed" => {
                "モデルの処理を停止しました。再検証してから有効にしてください。"
            }
            _ => "モデルを検証してから有効にしてください。",
        }
    }
}
pub fn status() -> Result<Status> {
    Ok(serde_json::from_value(request(
        Action::LlmStatus,
        serde_json::Value::Null,
    )?)?)
}
fn request(action: Action, value: serde_json::Value) -> Result<serde_json::Value> {
    crate::manage(Manage {
        action: action as i32,
        json: if value.is_null() {
            String::new()
        } else {
            serde_json::to_string(&value)?
        },
        ..Default::default()
    })
}
pub fn import(
    path: &Path,
    model: &CatalogEntry,
    license: bool,
    local: bool,
) -> Result<serde_json::Value> {
    ensure!(path.is_absolute(), "model path must be absolute");
    ensure!(
        license && local,
        "model license and local processing consent are required"
    );
    #[derive(Serialize)]
    struct Import<'a> {
        path: &'a Path,
        model: &'a str,
        license: &'a str,
        terms_sha256: &'a str,
        accept_license: bool,
        accept_local_processing: bool,
    }
    request(
        Action::LlmImport,
        serde_json::to_value(Import {
            path,
            model: &model.id,
            license: &model.license,
            terms_sha256: &model.terms_sha256,
            accept_license: license,
            accept_local_processing: local,
        })?,
    )
}
pub fn qualify() -> Result<serde_json::Value> {
    request(Action::LlmQualify, serde_json::json!({}))
}
pub fn remove() -> Result<serde_json::Value> {
    request(Action::LlmRemove, serde_json::json!({}))
}
const USAGE: &str = "usage: kanade-ui models [status|import FILE [--model ID] --accept-license --accept-local-processing|qualify|remove]";

#[derive(Debug, PartialEq, Eq)]
enum Command<'a> {
    Status,
    Import {
        path: &'a Path,
        model: Option<&'a str>,
    },
    Qualify,
    Remove,
}
fn command(args: &[String]) -> Result<Command<'_>> {
    match args.first().map(String::as_str) {
        None => Ok(Command::Status),
        Some("status") if args.len() == 1 => Ok(Command::Status),
        Some("qualify") if args.len() == 1 => Ok(Command::Qualify),
        Some("remove") if args.len() == 1 => Ok(Command::Remove),
        Some("import") => {
            let mut path = None;
            let mut model = None;
            let mut license = false;
            let mut local = false;
            let mut args = args[1..].iter();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--model" => {
                        ensure!(model.is_none(), "model option specified twice");
                        let value = args
                            .next()
                            .filter(|id| !id.is_empty() && !id.starts_with('-'));
                        model = Some(
                            value
                                .ok_or_else(|| anyhow::anyhow!("model ID required"))?
                                .as_str(),
                        );
                    }
                    "--accept-license" => {
                        ensure!(!license, "license consent specified twice");
                        license = true;
                    }
                    "--accept-local-processing" => {
                        ensure!(!local, "local processing consent specified twice");
                        local = true;
                    }
                    value if !value.starts_with('-') && path.is_none() => {
                        path = Some(Path::new(value))
                    }
                    _ => bail!(USAGE),
                }
            }
            let path = path.ok_or_else(|| anyhow::anyhow!("model path required"))?;
            ensure!(path.is_absolute(), "model path must be absolute");
            ensure!(
                license && local,
                "model license and local processing consent are required"
            );
            Ok(Command::Import { path, model })
        }
        _ => bail!(USAGE),
    }
}
fn catalog_model<'a>(catalog: &'a [CatalogEntry], id: Option<&str>) -> Result<&'a CatalogEntry> {
    if let Some(id) = id {
        return catalog
            .iter()
            .find(|model| model.id == id)
            .ok_or_else(|| anyhow::anyhow!("unsupported model ID"));
    }
    ensure!(
        catalog.len() <= 1,
        "multiple models available; select one with --model ID"
    );
    catalog
        .first()
        .ok_or_else(|| anyhow::anyhow!("no supported model"))
}
pub fn run(args: &[String]) -> Result<()> {
    let result = match command(args)? {
        Command::Status => request(Action::LlmStatus, serde_json::Value::Null)?,
        Command::Import { path, model } => {
            let state = status()?;
            import(path, catalog_model(&state.catalog, model)?, true, true)?
        }
        Command::Qualify => qualify()?,
        Command::Remove => remove()?,
    };
    if let Some(outcome) = result["result"].as_str() {
        ensure!(
            matches!(outcome, "started" | "removed" | "not_installed"),
            "model operation refused: {outcome}"
        );
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<CatalogEntry> {
        ["fixture-a", "fixture-b"]
            .into_iter()
            .map(|id| CatalogEntry {
                id: id.into(),
                name: id.into(),
                size: 1,
                license: "fixture".into(),
                license_url: "https://example.com/license".into(),
                source_url: "https://example.com/model".into(),
                attribution: "Example".into(),
                terms_sha256: format!("{id}-terms-v1"),
                profile: Some(format!("{id}-profile")),
                profile_sha256: Some(format!("{id}-profile-v1")),
            })
            .collect()
    }
    fn state() -> Status {
        Status {
            ready: false,
            enabled: false,
            runtime: "available".into(),
            state: "not_installed".into(),
            reason: "none".into(),
            catalog: catalog(),
            installed: None,
            job: None,
        }
    }
    fn accept(selection: &mut ModelSelection) {
        selection.license = true;
        selection.local_processing = true;
    }
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn initial_selection_prefers_the_installed_model_and_never_accepts_terms() {
        let mut state = state();
        state.installed = Some(InstalledModel {
            id: "fixture-b".into(),
        });
        let mut selection = ModelSelection::default();
        selection.refresh(Some(&state));
        let model = selection.selected(&state).unwrap();
        assert_eq!(model.id, "fixture-b");
        assert!(!selection.accepted(model));
    }
    #[test]
    fn changing_model_resets_both_consents_and_rejects_an_old_dialog_selection() {
        let state = state();
        let mut selection = ModelSelection::default();
        selection.refresh(Some(&state));
        let previous = selection.selected(&state).unwrap().clone();
        accept(&mut selection);
        assert!(selection.accepted(&previous));
        selection.select(Some(&state.catalog[1]));
        assert!(!selection.license && !selection.local_processing);
        accept(&mut selection);
        assert!(!selection.accepted(&previous));
        assert!(selection.accepted(&state.catalog[1]));
    }
    #[test]
    fn a_catalog_reorder_preserves_the_named_selection_and_consent() {
        let mut state = state();
        let mut selection = ModelSelection::default();
        selection.refresh(Some(&state));
        accept(&mut selection);
        state.catalog.reverse();
        selection.refresh(Some(&state));
        let model = selection.selected(&state).unwrap();
        assert_eq!(model.id, "fixture-a");
        assert!(selection.accepted(model));
    }
    #[test]
    fn changed_terms_require_fresh_consent_and_retire_an_open_dialog() {
        let mut state = state();
        let mut selection = ModelSelection::default();
        selection.refresh(Some(&state));
        let previous = selection.selected(&state).unwrap().clone();
        accept(&mut selection);
        state.catalog[0].terms_sha256 = "updated-terms".into();
        selection.refresh(Some(&state));
        assert!(!selection.license && !selection.local_processing);
        accept(&mut selection);
        assert!(!selection.accepted(&previous));
        assert!(selection.accepted(&state.catalog[0]));
    }
    #[test]
    fn changed_profile_retires_the_old_selection_even_if_the_license_is_unchanged() {
        let mut state = state();
        let mut selection = ModelSelection::default();
        selection.refresh(Some(&state));
        let previous = selection.selected(&state).unwrap().clone();
        accept(&mut selection);
        state.catalog[0].profile_sha256 = Some("updated-profile".into());
        selection.refresh(Some(&state));
        assert!(!selection.license && !selection.local_processing);
        accept(&mut selection);
        assert!(!selection.accepted(&previous));
        assert!(selection.accepted(&state.catalog[0]));
    }
    #[test]
    fn missing_catalog_or_connection_drops_consent_before_a_new_selection() {
        let mut state = state();
        let mut selection = ModelSelection::default();
        selection.refresh(Some(&state));
        accept(&mut selection);
        state.catalog.remove(0);
        selection.refresh(Some(&state));
        assert_eq!(selection.selected(&state).unwrap().id, "fixture-b");
        assert!(!selection.license && !selection.local_processing);
        accept(&mut selection);
        selection.refresh(None);
        assert!(selection.selected(&state).is_none());
        selection.refresh(Some(&state));
        assert!(!selection.accepted(&state.catalog[0]));
    }
    #[test]
    fn command_line_requires_an_unambiguous_registered_model() {
        let catalog = catalog();
        assert!(catalog_model(&catalog, None).is_err());
        assert_eq!(
            catalog_model(&catalog, Some("fixture-b")).unwrap().id,
            "fixture-b"
        );
        assert!(catalog_model(&catalog, Some("unknown")).is_err());
        assert!(catalog_model(&[], None).is_err());
        assert_eq!(catalog_model(&catalog[..1], None).unwrap().id, "fixture-a");
    }
    #[test]
    fn import_command_binds_the_explicit_model_and_both_consents() {
        let path = std::env::temp_dir().join("model.gguf");
        let path = path.to_str().unwrap();
        let values = args(&[
            "import",
            path,
            "--model",
            "fixture-b",
            "--accept-license",
            "--accept-local-processing",
        ]);
        assert_eq!(
            command(&values).unwrap(),
            Command::Import {
                path: Path::new(path),
                model: Some("fixture-b")
            }
        );
        let values = args(&[
            "import",
            path,
            "--accept-license",
            "--accept-local-processing",
        ]);
        assert_eq!(
            command(&values).unwrap(),
            Command::Import {
                path: Path::new(path),
                model: None
            }
        );
    }
    #[test]
    fn invalid_imports_are_rejected_before_contacting_the_engine() {
        let path = std::env::temp_dir().join("model.gguf");
        let path = path.to_str().unwrap();
        for values in [
            args(&["import", path]),
            args(&["import", path, "--accept-license"]),
            args(&[
                "import",
                "relative.gguf",
                "--accept-license",
                "--accept-local-processing",
            ]),
            args(&[
                "import",
                path,
                "--model",
                "--accept-license",
                "--accept-local-processing",
            ]),
            args(&[
                "import",
                path,
                "--model",
                "a",
                "--model",
                "b",
                "--accept-license",
                "--accept-local-processing",
            ]),
            args(&[
                "import",
                path,
                "--accept-license",
                "--accept-license",
                "--accept-local-processing",
            ]),
            args(&[
                "import",
                path,
                "--accept-license",
                "--accept-local-processing",
                "--unknown",
            ]),
            args(&[
                "import",
                path,
                path,
                "--accept-license",
                "--accept-local-processing",
            ]),
            args(&["remove", "--force"]),
        ] {
            assert!(command(&values).is_err());
        }
    }
}
