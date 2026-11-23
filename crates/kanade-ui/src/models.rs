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
    #[serde(default)]
    pub copied: u8,
}
#[derive(Clone, Deserialize)]
pub struct InstalledModel {
    pub id: String,
    /// The name of the model the file is. None when the file is not a model
    /// this version registers, even under the same ID.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub consent_current: bool,
}
impl InstalledModel {
    /// How the settings name the installed file.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("{}（同梱のモデルとは別のファイル）", self.id))
    }
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

impl Status {
    pub fn catalog_entry(&self) -> Option<&CatalogEntry> {
        self.catalog
            .iter()
            .find(|model| self.has_consent(model))
            .or_else(|| self.catalog.first())
    }
    pub fn has_consent(&self, model: &CatalogEntry) -> bool {
        self.installed
            .as_ref()
            .is_some_and(|installed| installed.id == model.id && installed.consent_current)
    }
    pub fn needs_import(&self) -> bool {
        self.catalog_entry()
            .is_none_or(|model| !self.has_consent(model))
            || matches!(self.reason.as_str(), "model_missing" | "model_changed")
    }
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
                "activating" => return "検証が完了しました。AI変換をオンにしています。",
                "failed" => {
                    return match job.error.as_str() {
                        "activation_failed" if self.enabled && self.ready => "AI変換はオンです。",
                        "activation_failed" => {
                            "検証は完了しました。設定を保存できなかったため、AI変換をオンにしてください。"
                        }
                        "source_unreadable" => {
                            "モデルのファイルを読み込めませんでした。ファイルを確認してもう一度お試しください。"
                        }
                        "storage" => {
                            "モデルを保存できませんでした。空き容量とアクセス権を確認してください。"
                        }
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
                "同意と検証は完了しています。いつでもAI変換をオンにできます。"
            };
        }
        match self.state.as_str() {
            "not_installed" => "利用条件に同意すると、モデルの準備を始めます。",
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
                "モデルの処理を停止しました。再検証から再開できます。"
            }
            _ => "AI変換を使うには、モデルの検証が必要です。",
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
    request(
        Action::LlmImport,
        import_payload(path, model, license, local)?,
    )
}
fn import_payload(
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
    Ok(serde_json::to_value(Import {
        path,
        model: &model.id,
        license: &model.license,
        terms_sha256: &model.terms_sha256,
        accept_license: license,
        accept_local_processing: local,
    })?)
}
pub fn import_and_activate(path: &Path, model: &CatalogEntry) -> Result<serde_json::Value> {
    request(
        Action::LlmImport,
        serde_json::json!({"import_and_activate": import_payload(path, model, true, true)?}),
    )
}
pub fn qualify_and_activate() -> Result<serde_json::Value> {
    request(Action::LlmQualify, serde_json::json!({"activate": true}))
}
pub fn set_enabled(enabled: bool) -> Result<serde_json::Value> {
    request(
        Action::SetSettings,
        serde_json::json!({"changes": {"llm": enabled}}),
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
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn status_displays_only_the_consent_saved_for_the_current_model() {
        let mut state = state();
        assert!(!state.has_consent(state.catalog_entry().unwrap()));
        state.installed = Some(
            serde_json::from_value(serde_json::json!({
                "id": "fixture-b", "name": "Fixture B", "consent_current": true
            }))
            .unwrap(),
        );
        // A freshly opened window has only the engine reply.
        assert_eq!(state.catalog_entry().unwrap().id, "fixture-b");
        assert!(state.has_consent(state.catalog_entry().unwrap()));
        assert!(!state.has_consent(&state.catalog[0]));
        assert!(!state.needs_import());
        state.reason = "model_missing".into();
        assert!(state.needs_import());
        assert!(state.has_consent(state.catalog_entry().unwrap()));
        state.installed.as_mut().unwrap().consent_current = false;
        assert!(state.needs_import());
        assert!(!state.has_consent(&state.catalog[1]));
    }
    #[test]
    fn an_installed_file_is_named_only_as_the_model_it_is() {
        let registered: InstalledModel =
            serde_json::from_str(r#"{"id":"fixture-a","name":"Fixture A"}"#).unwrap();
        assert_eq!(registered.label(), "Fixture A");
        // A previous version's file under the same ID is not named after the
        // model of this version.
        let previous: InstalledModel =
            serde_json::from_str(r#"{"id":"fixture-a","name":null}"#).unwrap();
        assert_eq!(
            previous.label(),
            "fixture-a（同梱のモデルとは別のファイル）"
        );
        let older_engine: InstalledModel = serde_json::from_str(r#"{"id":"fixture-a"}"#).unwrap();
        assert_eq!(older_engine.name, None);
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
