use super::text_field::TextField;
use super::*;
use gpui::Entity;
use kanade_indexer::{
    management::ScopeQuery,
    overrides::{Edit, Entry, Page, Query},
};

#[derive(Default)]
pub(super) struct ReadingPanel {
    pub(super) page: Option<Page>,
    editor: Option<Editor>,
}
struct Editor {
    query: ScopeQuery,
    identifier: Entity<TextField>,
    reading: Entity<TextField>,
    reset: bool,
    frozen: bool,
    reviewed: Option<(String, Option<String>)>,
}
impl Editor {
    fn edit(&self, cx: &App) -> Edit {
        Edit {
            query: self.query.clone(),
            identifier: self.identifier.read(cx).text(),
            reading: (!self.reset).then(|| self.reading.read(cx).text()),
        }
    }
    fn composing(&self, cx: &App) -> bool {
        self.identifier.read(cx).composing() || self.reading.read(cx).composing()
    }
}
impl ReadingPanel {
    pub(super) fn has_draft(&self) -> bool {
        self.editor.as_ref().is_some_and(|editor| !editor.frozen)
    }
}
impl Settings {
    fn reading_scope_current(&self, query: &ScopeQuery) -> bool {
        !self.project_pending
            && !self.project_jobs_busy()
            && self.projects.revision == query.revision
            && self
                .projects
                .projects
                .iter()
                .any(|p| p.id == query.id && p.approval == query.approval && p.enabled)
    }
    pub(super) fn open_project_readings(
        &mut self,
        scope: ScopeQuery,
        offset: u32,
        cx: &mut Context<Self>,
    ) {
        if self.project_pending || self.project_jobs_busy() || self.project_has_draft() {
            return;
        }
        self.project_pending = true;
        let task =
            cx.background_spawn(async move { crate::projects::readings(Query { scope, offset }) });
        cx.spawn(async move |view,cx| {
            let result=task.await;
            let _=view.update(cx,|this,cx| {
                this.project_pending=false;
                match result {
                    Ok(page)=> {this.project_scope.readings.page=Some(page);this.status="読みを上書きした識別子を表示しています。".into();}
                    Err(_)=> this.status="読みの一覧を取得できませんでした。プロジェクト一覧を更新して開き直してください。".into(),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    fn edit_project_reading(&mut self, entry: Option<Entry>, reset: bool, cx: &mut Context<Self>) {
        if self.project_pending
            || self.project_jobs_busy()
            || self.project_scope.readings.editor.is_some()
        {
            return;
        }
        let Some(page) = &self.project_scope.readings.page else {
            return;
        };
        if !self.reading_scope_current(&page.query.scope) {
            return;
        }
        let query = page.query.scope.clone();
        let existing = entry.is_some();
        let (identifier, reading) = entry
            .map(|entry| (entry.identifier, entry.reading))
            .unwrap_or_default();
        let identifier = cx.new(|cx| TextField::new("識別子", identifier, cx));
        let reading = cx.new(|cx| TextField::new("上書きする読み", reading, cx));
        for field in [&identifier, &reading] {
            cx.observe(field, |this, _, cx| {
                if let Some(editor) = &mut this.project_scope.readings.editor {
                    editor.reviewed = None;
                }
                cx.notify();
            })
            .detach();
        }
        if existing {
            identifier.update(cx, |field, cx| field.enable(false, cx));
        }
        if reset {
            reading.update(cx, |field, cx| field.enable(false, cx));
        }
        self.project_scope.readings.editor = Some(Editor {
            query,
            identifier,
            reading,
            reset,
            frozen: false,
            reviewed: None,
        });
        cx.notify();
    }
    fn save_project_reading(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.project_scope.readings.editor else {
            return;
        };
        let edit = editor.edit(cx);
        if !self.reading_scope_current(&editor.query)
            || editor.frozen
            || editor.composing(cx)
            || !edit.valid()
            || editor.reviewed.as_ref() != Some(&(edit.identifier.clone(), edit.reading.clone()))
        {
            return;
        }
        let editor = self
            .project_scope
            .readings
            .editor
            .as_mut()
            .expect("checked editor");
        editor.frozen = true;
        for field in [&editor.identifier, &editor.reading] {
            field.update(cx, |field, cx| field.enable(false, cx));
        }
        window.blur(cx);
        self.start_project_job(crate::projects::JobRequest::Reading { edit }, cx);
    }
    pub(super) fn project_has_draft(&self) -> bool {
        self.project_scope
            .editor
            .as_ref()
            .is_some_and(|editor| !editor.frozen)
            || self.project_scope.readings.has_draft()
    }
    pub(super) fn reading_body(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(page) = &self.project_scope.readings.page else {
            return div();
        };
        let available = self.reading_scope_current(&page.query.scope);
        let mut view = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(format!("{} の読みの調整", page.query.scope.id))
            .child(
                div()
                    .text_sm()
                    .child("識別子に対応するかな読みを指定します。保存すると辞書を再作成します。"),
            );
        if let Some(editor) = &self.project_scope.readings.editor {
            let edit = editor.edit(cx);
            let valid = available && !editor.frozen && !editor.composing(cx) && edit.valid();
            view = view.child(
                card()
                    .p_3()
                    .gap_2()
                    .child("識別子")
                    .child(editor.identifier.clone())
                    .child("上書きする読み")
                    .child(editor.reading.clone())
                    .child(if editor.reset {
                        "上書きを解除して、自動生成の読みに戻します。"
                    } else {
                        "ひらがな・カタカナで128文字以内にしてください。"
                    })
                    .when(editor.reviewed.is_some(), |view| {
                        view.child(format!(
                            "{} → {}",
                            edit.identifier,
                            edit.reading
                                .clone()
                                .unwrap_or_else(|| "自動生成の読み".into())
                        ))
                        .child(
                            button(
                                "save-project-reading",
                                "保存して辞書を再作成",
                                "確認した読みを保存して辞書を再作成する",
                            )
                            .inactive(!valid)
                            .on_click(cx.listener(
                                |this, _, window, cx| this.save_project_reading(window, cx),
                            )),
                        )
                    })
                    .child(
                        button(
                            "review-project-reading",
                            "変更内容を確認…",
                            "読みの変更内容を確認する",
                        )
                        .inactive(!valid)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this
                                .project_scope
                                .readings
                                .editor
                                .as_ref()
                                .is_some_and(|editor| this.reading_scope_current(&editor.query))
                            {
                                return;
                            }
                            if let Some(editor) = &mut this.project_scope.readings.editor {
                                let edit = editor.edit(cx);
                                if !editor.frozen && !editor.composing(cx) && edit.valid() {
                                    editor.reviewed = Some((edit.identifier, edit.reading));
                                    cx.notify();
                                }
                            }
                        })),
                    ),
            );
        } else {
            view = view.child(
                button(
                    "add-project-reading",
                    "読みを追加…",
                    "識別子の読みを追加する",
                )
                .inactive(!available)
                .on_click(cx.listener(|this, _, _, cx| this.edit_project_reading(None, false, cx))),
            );
            view = view.child(format!("上書き: {} 件", page.total));
            for (index, entry) in page.entries.iter().enumerate() {
                let edit = entry.clone();
                let reset = entry.clone();
                view = view.child(
                    card()
                        .p_3()
                        .gap_2()
                        .child(format!("{} → {}", entry.identifier, entry.reading))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    button(
                                        ("edit-reading", index),
                                        "編集…",
                                        "この識別子の読みを編集する",
                                    )
                                    .inactive(!available)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.edit_project_reading(Some(edit.clone()), false, cx)
                                        },
                                    )),
                                )
                                .child(
                                    button(
                                        ("reset-reading", index),
                                        "上書きを解除…",
                                        "この識別子の読みの上書きを解除する",
                                    )
                                    .inactive(!available)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.edit_project_reading(Some(reset.clone()), true, cx)
                                        },
                                    )),
                                ),
                        ),
                );
            }
            let previous = page.query.scope.clone();
            let next = previous.clone();
            let offset = page.query.offset;
            let has_next = offset as usize + 50 < page.total;
            view = view.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button(
                            "previous-readings",
                            "前の50件",
                            "読みの前のページを表示する",
                        )
                        .inactive(!available || offset == 0)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if offset > 0 {
                                this.open_project_readings(
                                    previous.clone(),
                                    offset.saturating_sub(50),
                                    cx,
                                );
                            }
                        })),
                    )
                    .child(
                        button("next-readings", "次の50件", "読みの次のページを表示する")
                            .inactive(!available || !has_next)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if has_next {
                                    this.open_project_readings(
                                        next.clone(),
                                        offset.saturating_add(50),
                                        cx,
                                    );
                                }
                            })),
                    ),
            );
        }
        view.child(
            button(
                "back-from-readings",
                "プロジェクト一覧へ戻る",
                "読みの調整からプロジェクト一覧へ戻る",
            )
            .on_click(cx.listener(|this, _, _, cx| this.leave_project_scope(false, cx))),
        )
    }
}
