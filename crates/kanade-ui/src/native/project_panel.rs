use super::text_field::TextField;
use super::*;
use crate::projects::ScopeDraft;
use gpui::{Entity, Focusable};
use kanade_indexer::management::{AppScope, Edit, EditState, Operation, ScopeQuery, Summary};

#[derive(Default)]
pub(super) struct ScopePanel {
    pub(super) readings: super::project_readings::ReadingPanel,
    pub(super) editor: Option<ScopeEditor>,
    pub(super) job: Option<crate::projects::Job>,
    pub(super) owned_job: bool,
    pub(super) monitoring: Option<String>,
    discard: bool,
    close_after_discard: bool,
    settings_after_discard: bool,
}
pub(super) struct ScopeEditor {
    pub(super) original: AppScope,
    pub(super) fields: Vec<Entity<TextField>>,
    pub(super) reviewed: Option<Vec<String>>,
    pub(super) frozen: bool,
    pub(super) creation: Option<Creation>,
}
pub(super) struct Creation {
    pub(super) folder: std::path::PathBuf,
    pub(super) id: Entity<TextField>,
    pub(super) preview: Option<crate::projects::Preview>,
}
impl ScopeEditor {
    pub(super) fn draft(&self, cx: &App) -> ScopeDraft {
        ScopeDraft {
            original: self.original.clone(),
            apps: self
                .fields
                .iter()
                .map(|field| field.read(cx).text())
                .collect(),
        }
    }
    pub(super) fn composing(&self, cx: &App) -> bool {
        self.fields.iter().any(|field| field.read(cx).composing())
            || self
                .creation
                .as_ref()
                .is_some_and(|creation| creation.id.read(cx).composing())
    }
}

impl Settings {
    fn scope_editor(&self, editor: &ScopeEditor, cx: &mut Context<Self>) -> gpui::Div {
        let validation = self.validate_project_editor(editor, cx);
        let editable = !editor.frozen && !self.project_pending && !self.project_jobs_busy();
        let can_review = editable && !editor.composing(cx) && validation.is_ok();
        let mut form = card()
            .p_4()
            .gap_3()
            .child(if editor.creation.is_some() {
                "フォルダを追加".to_string()
            } else {
                format!("{} の対象アプリ", editor.original.id)
            })
            .when_some(editor.creation.as_ref(), |form, creation| {
                form.child(div().child("プロジェクトID").child(creation.id.clone()))
            })
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(editor.original.root.clone()),
            )
            .child("登録したアプリで、このプロジェクトの識別子を変換候補に表示します。")
            .child(div().text_sm().text_color(rgb(MUTED)).child(
                "アプリを選択するか、アプリのIDを入力してください。macOSの例: com.example.Editor",
            ))
            .when(cfg!(target_os = "macos"), |form| {
                form.child(
                    button(
                        "scope-choose-app",
                        "アプリを選択…",
                        "アプリを選んで対象一覧に追加する",
                    )
                    .inactive(!editable || editor.fields.len() >= 64)
                    .on_click(cx.listener(|this, _, _, cx| {
                        #[cfg(target_os = "macos")]
                        this.choose_scope_app(cx);
                        #[cfg(not(target_os = "macos"))]
                        let _ = (this, cx);
                    })),
                )
            })
            .child(
                div()
                    .id("scope-fields")
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(editor.fields.iter().enumerate().map(|(index, field)| {
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().min_w_0().child(field.clone()))
                            .child(
                                button(
                                    format!("scope-remove-{index}"),
                                    "外す",
                                    "この対象アプリを一覧から外す",
                                )
                                .inactive(!editable)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        if this.project_pending {
                                            return;
                                        }
                                        if let Some(editor) = &mut this.project_scope.editor
                                            && !editor.frozen
                                            && index < editor.fields.len()
                                        {
                                            editor.fields.remove(index);
                                            editor.reviewed = None;
                                            if let Some(creation) = &mut editor.creation {
                                                creation.preview = None;
                                            }
                                            window.blur(cx);
                                            cx.notify();
                                        }
                                    },
                                )),
                            )
                    })),
            )
            .child(
                button(
                    "scope-add-app",
                    "アプリを追加",
                    "対象アプリの入力欄を追加する",
                )
                .inactive(!editable || editor.fields.len() >= 64)
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.project_pending
                        || this
                            .project_scope
                            .editor
                            .as_ref()
                            .is_none_or(|editor| editor.frozen || editor.fields.len() >= 64)
                    {
                        return;
                    }
                    let field = Self::scope_field(String::new(), cx);
                    window.focus(&field.focus_handle(cx), cx);
                    let editor = this.project_scope.editor.as_mut().expect("checked editor");
                    editor.fields.push(field);
                    editor.reviewed = None;
                    if let Some(creation) = &mut editor.creation {
                        creation.preview = None;
                    }
                    cx.notify();
                })),
            );
        let message = if editor.frozen {
            "送信済みの変更です。再送せず、一覧へ戻って現在の対象アプリを確認してください。"
        } else if editor.composing(cx) {
            "入力を確定してから変更内容を確認してください。"
        } else {
            validation
                .as_ref()
                .err()
                .copied()
                .unwrap_or("一覧を確認してから保存します。まだ変更は保存されていません。")
        };
        form = form
            .child(div().text_sm().text_color(rgb(MUTED)).child(message))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button(
                            "scope-review",
                            "変更内容を確認…",
                            "対象アプリの変更を確認する",
                        )
                        .inactive(!can_review)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.review_project_scope(cx);
                        })),
                    )
                    .child(
                        button(
                            "scope-back-list",
                            "一覧へ戻る",
                            "対象アプリの編集から一覧へ戻る",
                        )
                        .on_click(
                            cx.listener(|this, _, _, cx| this.leave_project_scope(false, cx)),
                        ),
                    ),
            );
        form
    }

    pub(super) fn project_panel(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let available = self.projects_loaded && !self.project_pending && !self.project_jobs_busy();
        let mut body = div()
            .id("project-list")
            .track_scroll(&self.project_scroll)
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .px_6()
            .pb_6()
            .child(control::scroll_begin());
        if self.project_scope.readings.page.is_some() {
            body = body.child(self.reading_body(cx));
        } else if let Some(editor) = &self.project_scope.editor {
            body = body.child(self.scope_editor(editor, cx));
        } else {
            body = body
                .child(
                    card()
                        .p_3()
                        .gap_2()
                        .child(format!(
                            "追加候補の表示: {}",
                            if !self.settings_loaded {
                                "未取得"
                            } else if self.config.project_dictionary {
                                "有効"
                            } else {
                                "無効"
                            }
                        ))
                        .child(div().text_sm().child(
                            "全体の表示が有効で、各プロジェクトも有効な場合に候補を追加します。",
                        ))
                        .child(
                            button(
                                "project-dictionary-global",
                                if !self.settings_loaded {
                                    "設定を再取得"
                                } else if self.config.project_dictionary {
                                    "追加候補を無効にする"
                                } else {
                                    "追加候補を有効にする"
                                },
                                "プロジェクト辞書の追加候補の表示を切り替える",
                            )
                            .inactive(self.loading || self.project_pending)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.project_pending {
                                    return;
                                }
                                if !this.settings_loaded {
                                    this.refresh(cx);
                                } else {
                                    this.update(|config| {
                                        config.project_dictionary = !config.project_dictionary
                                    });
                                }
                                cx.notify();
                            })),
                        ),
                )
                .child(
                    button(
                        "refresh-projects",
                        "一覧を更新",
                        "プロジェクト一覧を更新する",
                    )
                    .inactive(self.project_pending)
                    .on_click(cx.listener(|this, _, _, cx| this.load_projects(cx))),
                )
                .child(
                    button(
                        "project-add-folder",
                        "フォルダを追加…",
                        "辞書に使うフォルダを追加する",
                    )
                    .inactive(!available)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_project_folder(cx))),
                )
                .when(
                    self.projects_loaded && self.projects.projects.is_empty(),
                    |view| view.child("承認済みのプロジェクトはありません。"),
                );
            body = body.children(self.projects.projects.iter().map(|project| {
                let edit = Edit {
                    revision: self.projects.revision,
                    id: project.id.clone(),
                    approval: project.approval,
                    operation: Operation::SetEnabled {
                        enabled: !project.enabled,
                    },
                };
                let revoke = Edit {
                    operation: Operation::Revoke,
                    ..edit.clone()
                };
                let scope = project.clone();
                let reindex = ScopeQuery {
                    revision: self.projects.revision,
                    id: project.id.clone(),
                    approval: project.approval,
                };
                let enabled = project.enabled;
                let readings = reindex.clone();
                let monitor = Edit {
                    operation: Operation::SetMonitoring {
                        enabled: !project.monitoring,
                    },
                    ..edit.clone()
                };
                card()
                    .p_3()
                    .gap_2()
                    .child(format!(
                        "{}  {}",
                        project.id,
                        if project.enabled { "有効" } else { "無効" }
                    ))
                    .child(div().text_sm().child(project.root.clone()))
                    .child(div().text_sm().text_color(rgb(MUTED)).child(format!(
                        "対象アプリ: {} 件・読みの調整: {} 件",
                        project.apps, project.readings
                    )))
                    .child(div().text_sm().child(if !project.monitoring {
                        "変更の自動反映: オフ"
                    } else {
                        match self
                            .project_monitor
                            .as_ref()
                            .and_then(|report| {
                                report
                                    .projects
                                    .iter()
                                    .find(|status| status.id == project.id)
                            })
                            .map(|status| status.state)
                        {
                            Some(kanade_indexer::monitor::State::Watching) => {
                                "変更の自動反映: 監視中"
                            }
                            Some(kanade_indexer::monitor::State::Indexing) => {
                                "変更の自動反映: 辞書を更新中"
                            }
                            Some(kanade_indexer::monitor::State::Backoff) => {
                                "変更の自動反映: 再試行を待っています"
                            }
                            Some(kanade_indexer::monitor::State::Unavailable) => {
                                "変更の自動反映: フォルダの状態を確認してください"
                            }
                            _ if !project.enabled => {
                                "変更の自動反映: プロジェクトが無効のため停止中"
                            }
                            _ => "変更の自動反映: 状態を更新して確認してください",
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                button(
                                    format!("project-enabled-{}", project.id),
                                    if project.enabled {
                                        "無効にする"
                                    } else {
                                        "有効にする"
                                    },
                                    "このプロジェクトの有効状態を切り替える",
                                )
                                .inactive(!available)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.edit_project(edit.clone(), cx),
                                )),
                            )
                            .child(
                                button(
                                    format!("project-apps-{}", project.id),
                                    "対象アプリを編集…",
                                    "このプロジェクトの対象アプリを編集する",
                                )
                                .inactive(!available)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.open_project_scope(scope.clone(), cx)
                                    },
                                )),
                            )
                            .child(
                                button(
                                    format!("project-reindex-{}", project.id),
                                    "辞書を再作成",
                                    "このプロジェクトの辞書を再作成する",
                                )
                                .inactive(!available || !enabled)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.start_project_job(
                                            crate::projects::JobRequest::Index {
                                                query: reindex.clone(),
                                            },
                                            cx,
                                        )
                                    },
                                )),
                            )
                            .child(
                                button(
                                    format!("project-readings-{}", project.id),
                                    "読みを調整…",
                                    "このプロジェクトの読みを調整する",
                                )
                                .inactive(!available || !enabled)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if enabled {
                                            this.open_project_readings(readings.clone(), 0, cx);
                                        }
                                    },
                                )),
                            )
                            .child(
                                button(
                                    format!("project-monitor-{}", project.id),
                                    if project.monitoring {
                                        "自動反映をオフにする"
                                    } else {
                                        "変更を自動で反映する"
                                    },
                                    "このプロジェクトの変更の自動反映を切り替える",
                                )
                                .inactive(
                                    !available
                                        || (cfg!(windows) && !project.monitoring)
                                        || (!project.enabled && !project.monitoring),
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.edit_project(monitor.clone(), cx),
                                )),
                            )
                            .child(
                                button(
                                    format!("project-revoke-{}", project.id),
                                    "承認を解除…",
                                    "このプロジェクトの承認解除を確認する",
                                )
                                .text_color(rgb(DANGER))
                                .inactive(!available)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if this.projects_loaded && !this.project_pending {
                                            this.pending_project_revoke = Some(revoke.clone());
                                            cx.notify();
                                        }
                                    },
                                )),
                            ),
                    )
            }));
        }
        body = body.child(control::scroll_end(self.project_scroll.clone()));
        let mut confirmation = div().flex_none().px_6().gap_2().flex().flex_col();
        if self.project_scope.discard {
            confirmation = confirmation.child(
                card()
                    .p_3()
                    .gap_2()
                    .child("保存前のプロジェクト辞書の変更を破棄しますか？")
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                button(
                                    "scope-discard",
                                    "編集を破棄",
                                    "プロジェクト辞書の未保存編集を破棄する",
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        let close = this.project_scope.close_after_discard;
                                        let settings = this.project_scope.settings_after_discard;
                                        this.project_scope = Default::default();
                                        if settings {
                                            this.projects_open = false;
                                        }
                                        window.blur(cx);
                                        if close {
                                            cx.quit();
                                        } else {
                                            cx.notify();
                                        }
                                    },
                                )),
                            )
                            .child(
                                button("scope-keep", "編集を続ける", "対象アプリの編集に戻る")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.project_scope.discard = false;
                                        this.project_scope.close_after_discard = false;
                                        this.project_scope.settings_after_discard = false;
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        } else if let Some(editor) = &self.project_scope.editor
            && let Some(apps) = &editor.reviewed
        {
            confirmation = confirmation.child(
                card()
                    .p_3()
                    .gap_2()
                    .child(format!(
                        "{} の候補を、次の{}件のアプリで表示します。",
                        editor
                            .creation
                            .as_ref()
                            .map(|creation| creation.id.read(cx).text())
                            .unwrap_or_else(|| editor.original.id.clone()),
                        apps.len()
                    ))
                    .when_some(editor.creation.as_ref(), |view, creation| {
                        view.child(div().text_sm().child(creation.folder.display().to_string()))
                    })
                    .child(
                        div()
                            .id("scope-reviewed-apps")
                            .max_h(px(120.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(apps.iter().cloned().map(|app| div().text_sm().child(app))),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                button(
                                    "scope-save",
                                    if editor.creation.is_some() {
                                        "承認して辞書を作成"
                                    } else {
                                        "この対象で保存"
                                    },
                                    if editor.creation.is_some() {
                                        "フォルダを承認して辞書を作成する"
                                    } else {
                                        "確認した対象アプリを保存する"
                                    },
                                )
                                .inactive(
                                    self.project_pending || editor.frozen || editor.composing(cx),
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.save_project_scope(window, cx),
                                )),
                            )
                            .child(
                                button(
                                    "scope-revise",
                                    "編集に戻る",
                                    "保存せず対象アプリの編集に戻る",
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        if let Some(editor) = &mut this.project_scope.editor {
                                            editor.reviewed = None;
                                            if let Some(creation) = &mut editor.creation {
                                                creation.preview = None;
                                            }
                                        }
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            );
        }
        if let Some(edit) = &self.pending_project_revoke {
            let edit = edit.clone();
            confirmation = confirmation.child(card().p_3().bg(rgb(0xffecee)).gap_2()
                .child(format!("{} の承認を解除します。作成した辞書を削除し、元のフォルダは保持します。もう一度使う場合は「フォルダを追加」から承認し直してください。", edit.id))
                .child(div().flex().gap_2()
                    .child(button("confirm-project-revoke", "承認を解除する", "プロジェクトの承認を解除する")
                        .inactive(!available)
                        .on_click(cx.listener(move |this, _, _, cx| this.edit_project(edit.clone(), cx))))
                    .child(button("cancel-project-revoke", "キャンセル", "承認解除をキャンセルする")
                        .on_click(cx.listener(|this, _, _, cx| { this.pending_project_revoke = None; cx.notify(); })))));
        }
        div()
            .size_full()
            .bg(rgb(PAGE))
            .text_color(rgb(INK))
            .font_family(if cfg!(target_os = "macos") {
                "Hiragino Sans"
            } else {
                "Yu Gothic UI"
            })
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .p_6()
                    .gap_3()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_between()
                            .items_center()
                            .gap_2()
                            .child(app_heading("プロジェクト辞書", window))
                            .child(
                                button(
                                    "back-settings",
                                    "設定へ戻る",
                                    "プロジェクト辞書から設定へ戻る",
                                )
                                .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.leave_project_scope(true, cx)
                                    }),
                                ),
                            ),
                    )
                    .child(div().text_sm().child(
                        "承認済みのフォルダだけが対象です。対象アプリで変換候補を表示します。",
                    ))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(ACCENT))
                            .child(self.status.clone()),
                    ),
            )
            .child(self.project_job_section(cx))
            .child(confirmation)
            .child(body)
    }

    pub(super) fn confirm_project_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.project_has_draft() {
            self.projects_open = true;
            self.project_scope.discard = true;
            self.project_scope.close_after_discard = true;
            self.project_scope.settings_after_discard = false;
            self.status =
                "保存前のプロジェクト辞書の変更があります。編集を続けるか、破棄するか選んでください。".into();
            cx.notify();
            false
        } else {
            true
        }
    }
    pub(super) fn scope_field(value: String, cx: &mut Context<Self>) -> Entity<TextField> {
        let field = cx.new(|cx| TextField::new("対象アプリのID", value, cx));
        cx.observe(&field, |this, _, cx| {
            if let Some(editor) = &mut this.project_scope.editor {
                editor.reviewed = None;
                if let Some(creation) = &mut editor.creation {
                    creation.preview = None;
                }
            }
            cx.notify();
        })
        .detach();
        field
    }
    #[cfg(target_os = "macos")]
    fn choose_scope_app(&mut self, cx: &mut Context<Self>) {
        if self.project_pending || self.project_jobs_busy() {
            return;
        }
        let Some(editor) = &self.project_scope.editor else {
            return;
        };
        if editor.frozen || editor.fields.len() >= 64 {
            return;
        }
        let identity = editor.original.clone();
        self.project_pending = true;
        let chooser = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("候補を表示するアプリを選択".into()),
        });
        cx.spawn(async move |view, cx| {
            let chosen = match chooser.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let cancelled = chosen.is_none();
            let app = if let Some(path) = chosen {
                cx.background_executor().spawn(async move { macos::application_id(&path) }).await
            } else { None };
            let _ = view.update(cx, |this, cx| {
                this.project_pending = false;
                if let Some(editor) = &this.project_scope.editor
                    && !editor.frozen && editor.fields.len() < 64
                    && editor.original.id == identity.id
                    && editor.original.approval == identity.approval
                    && editor.original.revision == identity.revision
                    && editor.original.root == identity.root
                {
                    if let Some(app) = app {
                        if editor.fields.iter().any(|field| field.read(cx).text() == app) {
                            this.status = "選択したアプリは、すでに対象一覧にあります。".into();
                        } else {
                            let field = Self::scope_field(app, cx);
                            let editor = this.project_scope.editor.as_mut().expect("checked editor");
                            editor.fields.push(field);
                            editor.reviewed = None;
                    if let Some(creation) = &mut editor.creation { creation.preview = None; }
                            this.status = "一覧に追加しました。内容を確認して保存してください。".into();
                        }
                    } else if !cancelled {
                        this.status = "アプリのIDを取得できませんでした。.appを選択するか、IDを入力してください。".into();
                    }
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    fn open_project_scope(&mut self, project: Summary, cx: &mut Context<Self>) {
        if self.project_pending || self.project_scope.editor.is_some() || !self.projects_loaded {
            return;
        }
        let query = ScopeQuery {
            revision: self.projects.revision,
            id: project.id,
            approval: project.approval,
        };
        self.project_pending = true;
        self.pending_project_revoke = None;
        self.status = "対象アプリを取得しています。".into();
        let expected = query.clone();
        let task = cx.background_spawn(async move { crate::projects::scope(query) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.project_pending = false;
                match result {
                    Ok(scope) if scope.id == expected.id && scope.approval == expected.approval
                        && scope.revision == expected.revision => {
                        let fields = scope.apps.iter().map(|value| Self::scope_field(value.clone(), cx)).collect();
                        this.project_scope.editor = Some(ScopeEditor { original: scope, fields, reviewed: None, frozen: false, creation: None });
                        this.status.clear();
                    }
                    _ => this.status = "対象アプリを取得できませんでした。承認や設定が変更されている場合は一覧を更新してください。".into(),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    fn save_project_scope(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .project_scope
            .editor
            .as_ref()
            .is_some_and(|editor| editor.creation.is_some())
        {
            self.save_project_approval(window, cx);
            return;
        }
        if self.project_pending || self.project_jobs_busy() {
            return;
        }
        let Some(editor) = &self.project_scope.editor else {
            return;
        };
        let draft = editor.draft(cx);
        if editor.frozen || editor.composing(cx) || editor.reviewed.as_ref() != Some(&draft.apps) {
            return;
        }
        let Ok(edit) = draft.edit(&self.projects) else {
            return;
        };
        let editor = self.project_scope.editor.as_mut().expect("checked editor");
        editor.frozen = true;
        for field in &editor.fields {
            field.update(cx, |field, cx| field.enable(false, cx));
        }
        window.blur(cx);
        self.project_pending = true;
        self.status = "対象アプリの変更を保存しています。".into();
        let identity = draft.original.clone();
        let task = cx.background_spawn(async move {
            // Submit exactly once. A lost reply is resolved by readback, never replay.
            let result = crate::projects::edit(edit);
            let latest = crate::projects::list();
            let observed = latest.as_ref().ok().and_then(|listing| {
                listing
                    .projects
                    .iter()
                    .find(|project| {
                        project.id == identity.id && project.approval == identity.approval
                    })
                    .and_then(|project| {
                        crate::projects::scope(ScopeQuery {
                            revision: listing.revision,
                            id: project.id.clone(),
                            approval: project.approval,
                        })
                        .ok()
                    })
            });
            (result, latest, observed)
        });
        cx.spawn(async move |view, cx| {
            let (result, latest, observed) = task.await;
            let _ = view.update(cx, |this, cx| {
                this.project_pending = false;
                this.projects_loaded = false;
                if let Ok(listing) = latest {
                    this.projects = listing;
                    this.projects_loaded = true;
                }
                let matched = observed.as_ref().is_some_and(|current| draft.matches(current));
                this.status = match &result {
                    Ok(reply) if reply.result == EditState::Applied && matched && reply.reloaded => "保存して入力エンジンに反映しました。",
                    Ok(reply) if reply.result == EditState::Applied && matched => "対象アプリは保存済みですが、入力エンジンへの反映を確認できませんでした。",
                    Ok(reply) if matches!(reply.result, EditState::Stale | EditState::NotFound) => "承認や設定が変わっていたため保存しませんでした。一覧から編集を開き直してください。",
                    _ if matched => "現在の対象一覧は指定した内容です。保存の応答とエンジンへの反映は確認できませんでした。変更は再送していません。",
                    _ => "変更結果を確認できませんでした。変更は再送していません。一覧へ戻り、対象アプリを開き直してください。",
                }.into();
                if matched {
                    this.project_scope.editor = None;
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    pub(super) fn leave_project_scope(&mut self, settings: bool, cx: &mut Context<Self>) {
        if self.project_has_draft() {
            self.project_scope.discard = true;
            self.project_scope.close_after_discard = false;
            self.project_scope.settings_after_discard = settings;
        } else {
            let job = self.project_scope.job.take();
            let monitoring = self.project_scope.monitoring.take();
            self.project_scope = ScopePanel {
                job,
                monitoring,
                ..Default::default()
            };
            if settings {
                self.projects_open = false;
            }
        }
        cx.notify();
    }
    pub(super) fn load_projects(&mut self, cx: &mut Context<Self>) {
        if self.project_pending {
            return;
        }
        self.project_pending = true;
        self.pending_project_revoke = None;
        self.status = "プロジェクトを取得しています。".into();
        let task = cx.background_spawn(async {
            (
                crate::projects::list(),
                crate::projects::jobs(0),
                crate::projects::monitoring(),
            )
        });
        cx.spawn(async move |view, cx| {
            let (result, jobs, monitor) = task.await;
            let _ = view.update(cx, |this, cx| {
                this.project_pending = false;
                this.attach_project_jobs(jobs.ok(), cx);
                this.project_monitor = monitor.ok();
                match result {
                    Ok(registry) => {
                        this.projects = registry;
                        this.projects_loaded = true;
                        this.status.clear();
                    }
                    Err(_) => {
                        this.status =
                            "プロジェクトを取得できませんでした。エンジンの接続を確認してください。"
                                .into()
                    }
                }
                this.monitor_source_status(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn edit_project(&mut self, edit: kanade_indexer::management::Edit, cx: &mut Context<Self>) {
        if self.project_pending || self.project_jobs_busy() {
            return;
        }
        self.project_pending = true;
        self.pending_project_revoke = None;
        self.status = "プロジェクトの変更を保存しています。".into();
        let expected = edit.clone();
        let task = cx.background_spawn(async move {
            let reply = crate::projects::edit(edit);
            let latest = crate::projects::list();
            (reply, latest)
        });
        cx.spawn(async move |view, cx| {
            let (result, latest) = task.await;
            let _ = view.update(cx, |this, cx| {
                use kanade_indexer::management::{EditState, Operation};
                this.project_pending = false;
                if let Ok(listing) = latest {
                    this.projects = listing;
                    this.projects_loaded = true;
                } else if let Ok(update) = &result && let Some(listing) = &update.listing {
                    this.projects = listing.clone();
                    this.projects_loaded = true;
                }
                let matched = this.projects_loaded && match expected.operation {
                    Operation::Revoke => !this.projects.projects.iter().any(|p| p.id == expected.id),
                    Operation::SetEnabled { enabled } => this.projects.projects.iter().any(|p| p.id == expected.id && p.approval == expected.approval && p.enabled == enabled),
                    Operation::SetMonitoring { enabled } => this.projects.projects.iter().any(|p| p.id == expected.id && p.approval == expected.approval && p.monitoring == enabled),
                    Operation::SetApps { .. } => false,
                };
                match result {
                    Ok(update) => {
                        this.status = match update.result {
                            EditState::Applied if !matched => "変更は保存済みですが、現在の一覧が変わりました。内容を確認してください。",
                            EditState::Applied if !update.cleanup_complete => "承認解除は保存しました。辞書ファイルを安全に削除できなかったため保持しています。",
                            EditState::Applied if !update.reloaded => "保存しましたが、入力エンジンへの反映を確認できませんでした。再接続後に更新してください。",
                            EditState::Applied => "保存して入力エンジンに反映しました。",
                            EditState::Stale | EditState::NotFound => "一覧が変わっていたため変更しませんでした。更新した一覧を確認してください。",
                            EditState::Busy => "別の操作で更新中です。少し待ってから、もう一度操作してください。",
                            _ => "保存を確認できませんでした。一覧を更新してから、もう一度操作してください。",
                        }.into();
                    }
                    Err(_) if matched => this.status = "変更は保存済みです。入力エンジンへの反映は確認できませんでした。".into(),
                    Err(_) => this.status = "変更を確認できませんでした。一覧を更新してから、もう一度操作してください。".into(),
                }
                this.monitor_source_status(cx);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
}
