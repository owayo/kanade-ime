use super::text_field::TextField;
use super::*;
use gpui::{Entity, Focusable};
use kanade_engine::config::LearningExclusionsEdit;

pub(super) struct Editor {
    epoch: u64,
    previous: Vec<String>,
    fields: Vec<Entity<TextField>>,
    reviewed: Option<Vec<String>>,
    frozen: bool,
    discard: bool,
    close_after_discard: bool,
}
impl Editor {
    fn draft(&self, cx: &App) -> LearningExclusionsEdit {
        LearningExclusionsEdit {
            previous: self.previous.clone(),
            apps: self
                .fields
                .iter()
                .map(|field| field.read(cx).text())
                .collect(),
        }
    }
    fn composing(&self, cx: &App) -> bool {
        self.fields.iter().any(|field| field.read(cx).composing())
    }
}
impl Settings {
    fn discard_learning_apps(&mut self, cx: &mut Context<Self>) {
        if self.learning_busy {
            return;
        }
        let Some(editor) = self.learning_editor.take() else {
            return;
        };
        if editor.close_after_discard {
            cx.quit();
        } else {
            self.status = if editor.frozen {
                "編集を閉じました。送信済みの変更は取り消していません。"
            } else {
                "編集を破棄しました。"
            }
            .into();
            cx.notify();
        }
    }
    pub(super) fn learning_close_notice(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        if !self.learning_editor.as_ref()?.discard {
            return None;
        }
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .bg(rgb(0xffecee))
                .child("保存していない除外アプリの編集を破棄しますか？")
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            button(
                                "keep-learning-apps",
                                "編集を続ける",
                                "除外アプリの編集を続ける",
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(editor) = &mut this.learning_editor {
                                    editor.discard = false;
                                    editor.close_after_discard = false;
                                }
                                cx.notify();
                            })),
                        )
                        .child(
                            button(
                                "close-discard-learning-apps",
                                "破棄して閉じる",
                                "除外アプリの編集を破棄して設定を閉じる",
                            )
                            .inactive(self.learning_busy)
                            .on_click(cx.listener(|this, _, _, cx| this.discard_learning_apps(cx))),
                        ),
                ),
        )
    }
    fn learning_app_field(value: String, cx: &mut Context<Self>) -> Entity<TextField> {
        let field = cx.new(|cx| TextField::new("学習しないアプリのID", value, cx));
        cx.observe(&field, |this, _, cx| {
            if let Some(editor) = &mut this.learning_editor {
                editor.reviewed = None;
            }
            cx.notify();
        })
        .detach();
        field
    }
    fn open_learning_apps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready() || self.learning_editor.is_some() {
            return;
        }
        self.learning_epoch = self.learning_epoch.saturating_add(1);
        let fields: Vec<_> = self
            .config
            .learning_excluded_apps
            .iter()
            .map(|app| Self::learning_app_field(app.clone(), cx))
            .collect();
        if let Some(field) = fields.first() {
            window.focus(&field.read(cx).focus_handle(cx), cx);
        }
        self.learning_editor = Some(Editor {
            epoch: self.learning_epoch,
            previous: self.config.learning_excluded_apps.clone(),
            fields,
            reviewed: None,
            frozen: false,
            discard: false,
            close_after_discard: false,
        });
        cx.notify();
    }
    pub(super) fn confirm_learning_close(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(editor) = &mut self.learning_editor
            && !editor.frozen
        {
            editor.discard = true;
            editor.close_after_discard = true;
            cx.notify();
            return false;
        }
        true
    }
    fn save_learning_apps(&mut self, cx: &mut Context<Self>) {
        if self.learning_busy {
            return;
        }
        let Some(editor) = &mut self.learning_editor else {
            return;
        };
        let edit = editor.draft(cx);
        if editor.frozen
            || editor.composing(cx)
            || editor.reviewed.as_ref() != Some(&edit.apps)
            || edit.apply(&self.config).is_err()
        {
            return;
        }
        editor.frozen = true;
        for field in &editor.fields {
            field.update(cx, |field, cx| field.enable(false, cx));
        }
        let epoch = editor.epoch;
        let target = edit.apps.clone();
        self.learning_busy = true;
        let task = cx.background_spawn(async move {
            let result = crate::manage(Manage {
                action: Action::SetLearningExclusions as i32,
                json: serde_json::to_string(&edit)?,
                ..Default::default()
            });
            // An uncertain response is read back, never replayed.
            Ok::<_, anyhow::Error>((result, crate::settings()))
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.learning_busy = false;
                let current = result.ok().and_then(|(_, current)| current.ok());
                let applied = current.as_ref().is_some_and(|config| config.learning_excluded_apps == target);
                if let Some(current) = current { this.config = current; this.settings_loaded = true; }
                if this.learning_editor.as_ref().is_some_and(|editor| editor.epoch == epoch) && applied {
                    this.learning_editor = None;
                    this.status = "学習しないアプリを保存しました。入力先へ戻り、必要なら英数・かなを切り替えてください。".into();
                } else {
                    this.status = "保存結果が変わったか、確認できませんでした。「編集を破棄」で閉じ、一覧を更新して確認してください。自動では再送しません。".into();
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    #[cfg(target_os = "macos")]
    fn choose_learning_app(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.learning_editor else {
            return;
        };
        if editor.frozen || self.learning_busy || editor.fields.len() >= 64 {
            return;
        }
        let epoch = editor.epoch;
        self.learning_busy = true;
        let chooser = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("学習しないアプリを選択".into()),
        });
        cx.spawn(async move |view, cx| {
            let path = match chooser.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let app = if let Some(path) = path {
                cx.background_executor()
                    .spawn(async move { macos::application_id(&path) })
                    .await
            } else {
                None
            };
            let _ = view.update(cx, |this, cx| {
                this.learning_busy = false;
                if let Some(app) = app
                    && this.learning_editor.as_ref().is_some_and(|editor| {
                        editor.epoch == epoch
                            && !editor.frozen
                            && editor.fields.len() < 64
                            && !editor
                                .fields
                                .iter()
                                .any(|field| field.read(cx).text() == app)
                    })
                {
                    let field = Self::learning_app_field(app, cx);
                    if let Some(editor) = &mut this.learning_editor {
                        editor.fields.push(field);
                        editor.reviewed = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn learning_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut body = card().p_3().gap_3()
            .child(div().text_sm().text_color(rgb(MUTED)).child(
                "通常はすべてのアプリで、選んで確定した語と訂正した語を端末内に記録します。ここに追加したアプリでは学習しません。プライベート入力を判別できないアプリも除外できます。",
            ));
        if let Some(editor) = &self.learning_editor {
            let editable = !editor.frozen && !self.learning_busy;
            body = body.children(editor.fields.iter().map(|field| {
                let id = field.entity_id();
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(field.clone())
                    .child(
                        button(
                            ("remove-learning-app", id),
                            "除外を解除",
                            "このアプリの学習除外を解除する",
                        )
                        .inactive(!editable)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.learning_busy
                                && let Some(editor) = &mut this.learning_editor
                                && !editor.frozen
                            {
                                editor.fields.retain(|field| field.entity_id() != id);
                                editor.reviewed = None;
                                cx.notify();
                            }
                        })),
                    )
            }));
            body = body.child(
                div().flex().gap_2().flex_wrap().child(
                    button(
                        "add-learning-app",
                        "IDの入力欄を追加",
                        "除外アプリの入力欄を追加する",
                    )
                    .inactive(!editable || editor.fields.len() >= 64)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.learning_busy
                            || this
                                .learning_editor
                                .as_ref()
                                .is_none_or(|editor| editor.frozen || editor.fields.len() >= 64)
                        {
                            return;
                        }
                        let field = Self::learning_app_field(String::new(), cx);
                        window.focus(&field.read(cx).focus_handle(cx), cx);
                        if let Some(editor) = &mut this.learning_editor {
                            editor.fields.push(field);
                            editor.reviewed = None;
                        }
                        cx.notify();
                    })),
                ),
            );
            #[cfg(target_os = "macos")]
            {
                body = body.child(
                    button(
                        "choose-learning-app",
                        "アプリを選択…",
                        "学習しないアプリを選択する",
                    )
                    .inactive(!editable || editor.fields.len() >= 64)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_learning_app(cx))),
                );
            }
            let valid =
                editable && !editor.composing(cx) && editor.draft(cx).apply(&self.config).is_ok();
            body = body.child(
                button(
                    "review-learning-apps",
                    "除外アプリを確認…",
                    "除外アプリの変更内容を確認する",
                )
                .inactive(!valid)
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.learning_busy
                        && let Some(editor) = &mut this.learning_editor
                    {
                        let edit = editor.draft(cx);
                        if !editor.frozen
                            && !editor.composing(cx)
                            && edit.apply(&this.config).is_ok()
                        {
                            editor.reviewed = Some(edit.apps);
                        }
                    }
                    cx.notify();
                })),
            );
            if let Some(apps) = &editor.reviewed {
                body = body
                    .child(div().p_3().bg(rgb(0xeaf3ff)).child(if apps.is_empty() {
                        "除外をすべて解除し、すべてのアプリで学習します。".to_string()
                    } else {
                        format!("この一覧のアプリでは学習しません：{}", apps.join("、"))
                    }))
                    .child(
                        button(
                            "save-learning-apps",
                            "この除外設定で保存",
                            "確認した除外アプリを保存する",
                        )
                        .inactive(!valid)
                        .on_click(cx.listener(|this, _, _, cx| this.save_learning_apps(cx))),
                    );
            }
            body = body.child(
                button(
                    "discard-learning-apps",
                    "編集を破棄",
                    "保存前の除外アプリの編集を破棄する",
                )
                .inactive(self.learning_busy)
                .on_click(cx.listener(|this, _, _, cx| this.discard_learning_apps(cx))),
            );
        } else {
            body = body
                .child(if !self.settings_loaded {
                    "未取得".into()
                } else if self.config.learning_excluded_apps.is_empty() {
                    "除外なし（すべてのアプリで学習）".into()
                } else {
                    self.config.learning_excluded_apps.join("、")
                })
                .child(
                    button(
                        "edit-learning-apps",
                        "除外アプリを編集…",
                        "学習しないアプリを編集する",
                    )
                    .inactive(!self.ready())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_learning_apps(window, cx)),
                    ),
                );
        }
        section("学習しないアプリ").child(body)
    }
}
