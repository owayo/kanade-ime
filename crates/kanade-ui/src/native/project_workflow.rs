use super::project_panel::{Creation, ScopeEditor};
use super::text_field::TextField;
use super::*;
use crate::projects::{self, ApprovalRequest, JobRequest, Stage};
use kanade_indexer::management::{AppScope, valid_apps};
use std::time::{Duration, Instant};

impl Settings {
    pub(super) fn monitor_source_status(&mut self, cx: &mut Context<Self>) {
        if self.project_monitor_polling
            || !self.projects_open
            || !self
                .projects
                .projects
                .iter()
                .any(|p| p.enabled && p.monitoring)
        {
            return;
        }
        self.project_monitor_polling = true;
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let keep = view.update(cx, |this, _| {
                    this.projects_open
                        && this
                            .projects
                            .projects
                            .iter()
                            .any(|p| p.enabled && p.monitoring)
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
                let result = cx
                    .background_executor()
                    .spawn(async { projects::monitoring() })
                    .await;
                let keep = view.update(cx, |this, cx| {
                    this.project_monitor = result.ok();
                    cx.notify();
                    this.projects_open
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
            let _ = view.update(cx, |this, _| this.project_monitor_polling = false);
        })
        .detach();
    }
    pub(super) fn project_jobs_busy(&self) -> bool {
        self.project_scope
            .job
            .as_ref()
            .is_some_and(|job| !job.finished())
    }
    pub(super) fn validate_project_editor(
        &self,
        editor: &ScopeEditor,
        cx: &App,
    ) -> Result<(), &'static str> {
        if let Some(creation) = &editor.creation {
            let id = creation.id.read(cx).text();
            if !kanade_indexer::valid_id(&id) {
                return Err(
                    "プロジェクトIDは英数字・ハイフン・アンダースコアで64文字以内にしてください。",
                );
            }
            if self.projects.revision != editor.original.revision
                || self
                    .projects
                    .projects
                    .iter()
                    .any(|project| project.id == id)
            {
                return Err(
                    "一覧が変わったか、同じIDが使われています。一覧を更新して確認してください。",
                );
            }
            if !valid_apps(&editor.draft(cx).apps) {
                return Err("候補を表示するアプリを1〜64件指定してください。空欄は使えません。");
            }
            Ok(())
        } else {
            editor.draft(cx).edit(&self.projects).map(|_| ())
        }
    }
    pub(super) fn choose_project_folder(&mut self, cx: &mut Context<Self>) {
        if self.project_pending
            || self.project_jobs_busy()
            || !self.projects_loaded
            || self.project_scope.editor.is_some()
        {
            return;
        }
        self.project_pending = true;
        let revision = self.projects.revision;
        let chooser = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("辞書に使うフォルダを選択".into()),
        });
        cx.spawn(async move |view, cx| {
            let folder = match chooser.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let folder = if let Some(path) = folder {
                cx.background_executor()
                    .spawn(async move { path.canonicalize().ok() })
                    .await
            } else {
                None
            };
            let _ = view.update(cx, |this, cx| {
                this.project_pending = false;
                if let Some(folder) = folder
                    && this.projects.revision == revision
                {
                    let mut id: String = folder
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                        .take(55)
                        .collect();
                    if id.is_empty() {
                        id = "project".into();
                    }
                    let stem = id.clone();
                    let mut sequence = 1;
                    while this
                        .projects
                        .projects
                        .iter()
                        .any(|project| project.id == id)
                    {
                        sequence += 1;
                        id = format!("{stem}-{sequence}");
                    }
                    let id_field = cx.new(|cx| TextField::new("プロジェクトID", id.clone(), cx));
                    cx.observe(&id_field, |this, _, cx| {
                        if let Some(editor) = &mut this.project_scope.editor {
                            editor.reviewed = None;
                            if let Some(creation) = &mut editor.creation {
                                creation.preview = None;
                            }
                        }
                        cx.notify();
                    })
                    .detach();
                    let field = Self::scope_field(String::new(), cx);
                    this.project_scope.editor = Some(ScopeEditor {
                        original: AppScope {
                            revision,
                            id,
                            approval: 0,
                            root: folder.to_string_lossy().into_owned(),
                            apps: vec![],
                        },
                        fields: vec![field],
                        reviewed: None,
                        frozen: false,
                        creation: Some(Creation {
                            folder,
                            id: id_field,
                            preview: None,
                        }),
                    });
                    this.status =
                        "フォルダと対象アプリを確認してください。まだ承認も解析も行っていません。"
                            .into();
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn review_project_scope(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.project_scope.editor else {
            return;
        };
        if self.project_pending
            || self.project_jobs_busy()
            || editor.frozen
            || editor.composing(cx)
            || self.validate_project_editor(editor, cx).is_err()
        {
            return;
        }
        let apps = editor.draft(cx).apps;
        let Some(creation) = &editor.creation else {
            self.project_scope
                .editor
                .as_mut()
                .expect("checked editor")
                .reviewed = Some(apps);
            cx.notify();
            return;
        };
        let request = ApprovalRequest {
            revision: editor.original.revision,
            id: creation.id.read(cx).text(),
            root: creation.folder.clone(),
            apps,
        };
        let expected = request.clone();
        self.project_pending = true;
        let task = cx.background_spawn(async move { projects::prepare(request) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.project_pending = false;
                let Some(editor) = &this.project_scope.editor else { return; };
                let Some(creation) = &editor.creation else { return; };
                if editor.frozen || creation.id.read(cx).text() != expected.id || editor.draft(cx).apps != expected.apps || creation.folder != expected.root { return; }
                match result {
                    Ok(preview) if preview.id == expected.id && preview.revision == expected.revision && preview.apps == expected.apps && preview.root == expected.root => {
                        let editor = this.project_scope.editor.as_mut().expect("checked editor");
                        editor.reviewed = Some(preview.apps.clone());
                        editor.creation.as_mut().expect("creation editor").preview = Some(preview);
                        this.status = "このフォルダの識別子を解析し、選択したアプリで候補に使います。内容を確認して承認してください。".into();
                    }
                    _ => this.status = "承認内容を確認できませんでした。一覧・フォルダ・IDを確認してください。データ保存先と重なるフォルダは使えません。".into(),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    pub(super) fn save_project_approval(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.project_scope.editor else {
            return;
        };
        let Some(creation) = &editor.creation else {
            return;
        };
        let Some(preview) = &creation.preview else {
            return;
        };
        if self.project_pending
            || self.project_jobs_busy()
            || editor.frozen
            || editor.composing(cx)
            || self.validate_project_editor(editor, cx).is_err()
            || editor.reviewed.as_ref() != Some(&preview.apps)
            || editor.draft(cx).apps != preview.apps
            || creation.id.read(cx).text() != preview.id
            || creation.folder != preview.root
        {
            return;
        }
        let request = JobRequest::Approve {
            token: preview.token.clone(),
        };
        let editor = self.project_scope.editor.as_mut().expect("checked editor");
        editor.frozen = true;
        for field in &editor.fields {
            field.update(cx, |field, cx| field.enable(false, cx));
        }
        editor
            .creation
            .as_ref()
            .expect("creation editor")
            .id
            .update(cx, |field, cx| field.enable(false, cx));
        window.blur(cx);
        self.start_project_job(request, cx);
    }
    pub(super) fn start_project_job(&mut self, request: JobRequest, cx: &mut Context<Self>) {
        if self.project_pending || self.project_jobs_busy() {
            return;
        }
        self.project_pending = true;
        self.status = "辞書作成を開始しています。".into();
        let task = cx.background_spawn(async move {
            let known = match &request {
                JobRequest::Approve { token } => token.parse::<u64>().ok(),
                _ => None,
            };
            projects::start_job(request).or_else(|error| {
                // A preview already names its eventual job. Resolve a lost start
                // reply by reading that identity, without resubmitting the approval.
                known
                    .and_then(|id| projects::jobs(id).ok())
                    .and_then(|mut jobs| jobs.pop())
                    .ok_or(error)
            })
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.project_pending = false;
                match result {
                    Ok(job) => { this.status = projects::job_message(&job).into(); this.project_scope.job = Some(job); this.project_scope.owned_job = true; this.monitor_project_job(cx); }
                    Err(_) => { this.status = "開始結果を確認できませんでした。処理は再送していません。一覧と処理状況を更新してください。".into(); this.projects_loaded = false; }
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    pub(super) fn attach_project_jobs(
        &mut self,
        jobs: Option<Vec<projects::Job>>,
        cx: &mut Context<Self>,
    ) {
        let Some(jobs) = jobs else {
            return;
        };
        let current = self.project_scope.job.as_ref().map(|job| job.id.clone());
        let job = jobs
            .iter()
            .find(|job| !job.finished())
            .or_else(|| jobs.iter().find(|job| Some(&job.id) == current.as_ref()))
            .cloned();
        self.project_scope.owned_job &= job.as_ref().map(|job| &job.id) == current.as_ref();
        self.project_scope.job = job;
        if self.project_jobs_busy() {
            self.monitor_project_job(cx);
        }
    }
    fn monitor_project_job(&mut self, cx: &mut Context<Self>) {
        let Some(job) = &self.project_scope.job else {
            return;
        };
        if self.project_scope.monitoring.as_ref() == Some(&job.id) {
            return;
        }
        let identity = job.id.clone();
        let Ok(id) = identity.parse::<u64>() else {
            return;
        };
        self.project_scope.monitoring = Some(identity.clone());
        cx.spawn(async move |view, cx| {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let result = cx.background_executor().spawn(async move { projects::jobs(id) }).await;
                let keep = view.update(cx, |this, cx| {
                    if this.project_scope.job.as_ref().is_none_or(|job| job.id != identity) { return false; }
                    let job = result.ok().and_then(|jobs| jobs.into_iter().find(|job| job.id == identity));
                    let Some(job) = job else { this.status = "処理状況を取得できませんでした。処理は再送していません。「一覧・処理状況を更新」で確認してください。".into(); cx.notify(); return false; };
                    let finished = job.finished();
                    this.status = projects::job_message(&job).into();
                    if finished && this.project_scope.owned_job && (job.state == Stage::Ready || job.saved_revision.is_some()) {
                        this.project_scope.editor = None;
                        this.project_scope.readings = Default::default();
                    }
                    this.project_scope.job = Some(job);
                    if finished { this.project_scope.owned_job = false; this.load_projects(cx); }
                    cx.notify(); !finished
                });
                if !matches!(keep, Ok(true)) || Instant::now() >= deadline { break; }
                cx.background_executor().timer(Duration::from_millis(500)).await;
            }
            let _ = view.update(cx, |this, cx| {
                if this.project_scope.monitoring.as_ref() == Some(&identity) { this.project_scope.monitoring = None; cx.notify(); }
            });
        }).detach();
    }
    pub(super) fn project_job_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(job) = &self.project_scope.job else {
            return div();
        };
        let mut section = div()
            .flex_none()
            .px_6()
            .child(div().text_sm().child(projects::job_message(job)));
        section = section.child(
            button(
                "project-job-refresh",
                "一覧・処理状況を更新",
                "プロジェクトの処理状況を更新する",
            )
            .inactive(self.project_pending)
            .on_click(cx.listener(|this, _, _, cx| this.load_projects(cx))),
        );
        if !job.finished() {
            let id = job.id.parse::<u64>().ok();
            section = section.child(button("project-job-cancel", "辞書作成をキャンセル", "プロジェクトの辞書作成をキャンセルする")
                .inactive(self.project_pending).on_click(cx.listener(move |this, _, _, cx| {
                    if this.project_pending { return; }
                    let Some(id) = id else { return; };
                    this.project_pending = true;
                    let task = cx.background_spawn(async move { projects::cancel_job(id) });
                    cx.spawn(async move |view, cx| {
                        let result = task.await;
                        let _ = view.update(cx, |this, cx| {
                            this.project_pending = false;
                            this.status = if result.is_ok() { "キャンセルを要求しました。保存済みの承認は保持します。" } else { "すでに辞書の保存が始まったか、結果を確認できませんでした。処理状況を更新してください。" }.into();
                            this.monitor_project_job(cx); cx.notify();
                        });
                    }).detach(); cx.notify();
                })));
        }
        section
    }
}
