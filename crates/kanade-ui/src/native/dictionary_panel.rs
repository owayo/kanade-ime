use super::text_field::TextField;
use super::*;
use crate::user_dictionary::{self as dictionary, Draft, Edit, Entry, Job, Page, Stage, Target};
use gpui::{Entity, Focusable};
use std::time::{Duration, Instant};

struct Editor {
    draft: Draft,
    fields: Vec<Entity<TextField>>,
    frozen: bool,
    picker: Option<bool>,
}
impl Editor {
    fn new(draft: Draft, cx: &mut Context<Settings>) -> Self {
        let values = if matches!(draft.target, Target::Entry { .. }) {
            vec![
                ("読み", draft.reading.clone()),
                ("単語", draft.value.clone()),
                ("コメント", draft.comment.clone()),
            ]
        } else {
            vec![("辞書名", draft.name.clone())]
        };
        let fields = values
            .into_iter()
            .map(|(label, value)| {
                let field = cx.new(|cx| TextField::new(label, value, cx));
                cx.observe(&field, |_, _, cx| cx.notify()).detach();
                field
            })
            .collect();
        Self {
            draft,
            fields,
            frozen: false,
            picker: None,
        }
    }
    fn value(&self, cx: &App) -> Draft {
        let mut draft = self.draft.clone();
        if matches!(draft.target, Target::Entry { .. }) {
            draft.reading = self.fields[0].read(cx).text();
            draft.value = self.fields[1].read(cx).text();
            draft.comment = self.fields[2].read(cx).text();
        } else {
            draft.name = self.fields[0].read(cx).text();
        }
        draft
    }
    fn composing(&self, cx: &App) -> bool {
        self.fields.iter().any(|f| f.read(cx).composing())
    }
}
#[derive(Default)]
pub(super) struct DictionaryPanel {
    page: Option<Page>,
    editor: Option<Editor>,
    pending: bool,
    job: Option<Job>,
    owned_job: bool,
    generation: u64,
    offset: u32,
    message: String,
    confirm_delete: Option<(Edit, String)>,
    confirm_discard: bool,
    proposal: Option<(String, String)>,
    return_after_discard: bool,
    close_after_discard: bool,
    scroll: gpui::ScrollHandle,
}
impl DictionaryPanel {
    fn available(&self) -> bool {
        !self.pending && self.page.is_some() && self.job.as_ref().is_none_or(Job::finished)
    }
}
impl Settings {
    pub(super) fn confirm_settings_close(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.confirm_learning_close(cx) {
            return false;
        }
        if self
            .dictionary
            .editor
            .as_ref()
            .is_some_and(|editor| !editor.frozen)
        {
            self.dictionary_open = true;
            self.dictionary.confirm_discard = true;
            self.dictionary.close_after_discard = true;
            self.dictionary.return_after_discard = false;
            self.dictionary.message =
                "保存前の編集内容があります。破棄するか、編集を続けるか選んでください。".into();
            cx.notify();
            false
        } else {
            true
        }
    }
    pub(super) fn load_dictionary(&mut self, offset: u32, cx: &mut Context<Self>) {
        if self.dictionary.pending {
            return;
        }
        self.dictionary.pending = true;
        self.dictionary.confirm_delete = None;
        self.dictionary.generation = self.dictionary.generation.wrapping_add(1);
        let generation = self.dictionary.generation;
        let task =
            cx.background_spawn(async move { (dictionary::list(offset), dictionary::jobs()) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                if this.dictionary.generation != generation { return; }
                this.dictionary.pending = false;
                match result {
                    (Ok(page), Ok(jobs)) => {
                        this.dictionary.offset = page.offset;
                        this.dictionary.page = Some(page);
                        let job = dictionary::observed_job(this.dictionary.job.as_ref(),&jobs);
                        this.dictionary.owned_job &= job.as_ref().map(|j|&j.id)==this.dictionary.job.as_ref().map(|j|&j.id);
                        if let Some(job)=&job {
                            this.dictionary.message=job.message().into();
                            if job.state==Stage::Ready && this.dictionary.owned_job {
                                this.dictionary.editor=None;this.dictionary.owned_job=false;
                            }
                        }
                        this.dictionary.job = job;
                        if this.dictionary.job.as_ref().is_some_and(|j| !j.finished()) {
                            this.monitor_dictionary(cx);
                        } else if let Some((reading,value))=this.dictionary.proposal.take() {
                            let page=this.dictionary.page.as_ref().expect("loaded page");
                            let mut draft=Draft::new(page,None);
                            draft.reading=reading;draft.value=value;
                            this.dictionary.editor=Some(Editor::new(draft,cx));
                            this.dictionary.scroll.set_offset(gpui::point(px(0.),px(0.)));
                            this.dictionary.message="内容を確認して保存してください。".into();
                        }
                    }
                    _ => {
                        this.dictionary.page = None;
                        this.dictionary.message = "辞書を取得できませんでした。「一覧・処理状況を更新」で確認してください。".into();
                    }
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    fn edit_dictionary_draft(&mut self, draft: Draft, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dictionary.available() {
            return;
        }
        if self
            .dictionary
            .editor
            .as_ref()
            .is_some_and(|editor| !editor.frozen)
        {
            self.dictionary.message =
                "現在の編集を保存するか、キャンセルしてから別の編集を開いてください。".into();
            cx.notify();
            return;
        }
        let editor = Editor::new(draft, cx);
        window.focus(&editor.fields[0].focus_handle(cx), cx);
        self.dictionary.editor = Some(editor);
        self.dictionary
            .scroll
            .set_offset(gpui::point(px(0.), px(0.)));
        self.dictionary.job = None;
        self.dictionary.owned_job = false;
        self.dictionary.proposal = None;
        self.dictionary.confirm_delete = None;
        self.dictionary.confirm_discard = false;
        self.dictionary.return_after_discard = false;
        self.dictionary.close_after_discard = false;
        self.dictionary.message.clear();
        cx.notify();
    }
    pub(super) fn register_correction(
        &mut self,
        reading: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A displayed correction is a proposal, never an automatic promotion.
        if self
            .dictionary
            .editor
            .as_ref()
            .is_some_and(|editor| !editor.frozen)
        {
            self.dictionary_open = true;
            self.dictionary.message =
                "編集中の内容があります。保存するかキャンセルしてください。".into();
            cx.notify();
            return;
        }
        self.dictionary_open = true;
        if let Some(page) = &self.dictionary.page {
            let mut draft = Draft::new(page, None);
            draft.reading = reading;
            draft.value = value;
            self.edit_dictionary_draft(draft, window, cx);
        } else {
            self.dictionary.proposal = Some((reading, value));
            self.dictionary.message = "辞書を取得しています。".into();
            self.load_dictionary(0, cx);
        }
    }
    fn start_dictionary(&mut self, edit: Edit, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dictionary.available()
            || self
                .dictionary
                .page
                .as_ref()
                .is_none_or(|p| p.revision != edit.revision())
        {
            return;
        }
        if self
            .dictionary
            .editor
            .as_ref()
            .is_some_and(|e| e.composing(cx))
        {
            self.dictionary.message = "入力を確定してから保存してください。".into();
            cx.notify();
            return;
        }
        self.dictionary.pending = true;
        self.dictionary.confirm_delete = None;
        self.dictionary.confirm_discard = false;
        if let Some(editor) = &mut self.dictionary.editor {
            editor.frozen = true;
            for field in &editor.fields {
                field.update(cx, |field, cx| field.enable(false, cx));
            }
        }
        window.blur(cx);
        self.dictionary.generation = self.dictionary.generation.wrapping_add(1);
        let generation = self.dictionary.generation;
        self.dictionary.message = "変更を送信しています。".into();
        let task = cx.background_spawn(async move { dictionary::start(edit) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                if this.dictionary.generation != generation { return; }
                this.dictionary.pending = false;
                match result {
                    Ok(job) => {
                        this.dictionary.message = job.message().into();
                        this.dictionary.job = Some(job);
                        this.dictionary.owned_job = true;
                        this.monitor_dictionary(cx);
                    }
                    Err(_) => {
                        this.dictionary.message = "結果を確認できませんでした。同じ変更は再送していません。一覧・処理状況を更新して確認してください。".into();
                        this.dictionary.page = None;
                        this.dictionary.owned_job = false;
                    }
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    fn monitor_dictionary(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.dictionary.job.clone() else {
            return;
        };
        let generation = self.dictionary.generation;
        cx.spawn(async move |view, cx| {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let request = job.clone();
                let result = cx.background_executor().spawn(async move { dictionary::poll(&request) }).await;
                let keep = view.update(cx, |this, cx| {
                    if this.dictionary.generation != generation { return false; }
                    match result {
                        Ok(job) => {
                            let finished = job.finished();
                            this.dictionary.message = job.message().into();
                            if finished && job.state == Stage::Ready && this.dictionary.owned_job {
                                this.dictionary.editor = None;
                            }
                            this.dictionary.job = Some(job);
                            if finished {
                                this.dictionary.owned_job = false;
                                this.load_dictionary(this.dictionary.offset, cx);
                            }
                            cx.notify(); !finished
                        }
                        Err(_) => {
                            this.dictionary.message = "処理状況を確認できませんでした。変更は再送していません。状態を更新してください。".into();
                            cx.notify(); false
                        }
                    }
                });
                if !matches!(keep, Ok(true)) { break; }
                if Instant::now() >= deadline {
                    let _ = view.update(cx, |this, cx| {
                        if this.dictionary.generation == generation {
                            this.dictionary.message = "処理はまだ続いています。「一覧・処理状況を更新」で確認できます。".into(); cx.notify();
                        }
                    }); break;
                }
                cx.background_executor().timer(Duration::from_millis(500)).await;
            }
        }).detach();
    }
    fn cancel_dictionary(&mut self, cx: &mut Context<Self>) {
        if self.dictionary.pending {
            return;
        }
        let Some(job) = self.dictionary.job.clone().filter(|j| !j.finished()) else {
            return;
        };
        self.dictionary.pending = true;
        let task = cx.background_spawn(async move { dictionary::cancel(&job) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.dictionary.pending = false;
                this.dictionary.message = if result.is_ok() { "キャンセルを要求しました。処理状況を確認しています。" } else { "保存処理が始まったか、キャンセルを確認できませんでした。処理状況を更新してください。" }.into();
                this.load_dictionary(this.dictionary.offset,cx);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }
    fn dictionary_editor(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(editor) = &self.dictionary.editor else {
            return div();
        };
        let draft = editor.value(cx);
        let validation = self.dictionary.page.as_ref().map(|p| draft.validate(p));
        let ready = self.dictionary.available() && !editor.frozen && !editor.composing(cx);
        let can_save = ready && matches!(validation, Some(Ok(())));
        let target = draft.target.clone();
        let mut form =
            card()
                .p_4()
                .gap_3()
                .child(if matches!(target, Target::Entry { entry: Some(_), .. }) {
                    "単語を編集"
                } else if matches!(target, Target::Entry { .. }) {
                    "単語を登録"
                } else {
                    "辞書名"
                });
        for (index, field) in editor.fields.iter().enumerate() {
            let label = if matches!(target, Target::Entry { .. }) {
                ["読み", "単語", "コメント（任意）"][index]
            } else {
                "辞書名"
            };
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_sm().text_color(rgb(MUTED)).child(label))
                    .child(field.clone()),
            );
        }
        if let Target::Entry {
            dictionary: selected,
            entry,
        } = target
        {
            let name = self
                .dictionary
                .page
                .as_ref()
                .and_then(|p| p.dictionaries.iter().find(|d| Some(d.index) == selected))
                .map(|d| d.name.as_str())
                .unwrap_or("Kanade 手動登録（自動作成）");
            form = form.child(
                button(
                    "dictionary-target",
                    format!("保存先: {name}"),
                    "保存先の辞書を選ぶ",
                )
                .inactive(!ready || entry.is_some())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if ready
                        && entry.is_none()
                        && let Some(e) = &mut this.dictionary.editor
                    {
                        e.picker = if e.picker == Some(false) {
                            None
                        } else {
                            Some(false)
                        };
                        cx.notify();
                    }
                })),
            );
            let pos = dictionary::PARTS_OF_SPEECH
                .iter()
                .find(|(key, _)| *key == draft.pos)
                .map(|(_, label)| *label)
                .unwrap_or(&draft.pos);
            form = form.child(
                button("dictionary-pos", format!("品詞: {pos}"), "単語の品詞を選ぶ")
                    .inactive(!ready)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if ready && let Some(e) = &mut this.dictionary.editor {
                            e.picker = if e.picker == Some(true) {
                                None
                            } else {
                                Some(true)
                            };
                            cx.notify();
                        }
                    })),
            );
            if let Some(pos_picker) = editor.picker {
                let options: Vec<(String, String, Option<u32>)> = if pos_picker {
                    dictionary::PARTS_OF_SPEECH
                        .iter()
                        .map(|(key, label)| (key.to_string(), label.to_string(), None))
                        .collect()
                } else {
                    std::iter::once(("default".into(), "Kanade 手動登録（自動作成）".into(), None))
                        .chain(
                            self.dictionary
                                .page
                                .iter()
                                .flat_map(|p| p.dictionaries.iter())
                                .map(|d| (d.index.to_string(), d.name.clone(), Some(d.index))),
                        )
                        .collect()
                };
                form = form.child(
                    div()
                        .id("dictionary-options")
                        .max_h(px(150.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(options.into_iter().map(|(key, label, index)| {
                            button(
                                format!("dictionary-option-{key}"),
                                label,
                                "この選択肢を使う",
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if let Some(e) = &mut this.dictionary.editor
                                        && !e.frozen
                                    {
                                        if pos_picker {
                                            e.draft.pos = key.clone();
                                        } else if let Target::Entry {
                                            dictionary,
                                            entry: None,
                                        } = &mut e.draft.target
                                        {
                                            *dictionary = index;
                                        }
                                        e.picker = None;
                                        cx.notify();
                                    }
                                },
                            ))
                        })),
                );
            }
        }
        form.child(
            div()
                .text_sm()
                .text_color(rgb(MUTED))
                .child(if editor.composing(cx) {
                    "入力を確定してから保存してください。".into()
                } else if editor.frozen {
                    "送信済みの編集です。状態を確認してから一覧で開き直してください。".into()
                } else {
                    validation
                        .and_then(Result::err)
                        .unwrap_or("内容を確認して「保存」を押してください。")
                        .to_string()
                }),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .child(
                    button("dictionary-save", "保存", "辞書の変更を保存する")
                        .inactive(!can_save)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if !can_save {
                                return;
                            }
                            let Some(editor) = &this.dictionary.editor else {
                                return;
                            };
                            if editor.frozen || editor.composing(cx) {
                                return;
                            }
                            let Some(page) = &this.dictionary.page else {
                                return;
                            };
                            if let Ok(edit) = editor.value(cx).edit(page) {
                                this.start_dictionary(edit, window, cx);
                            }
                        })),
                )
                .child(
                    button("dictionary-discard", "キャンセル", "編集中の内容を破棄する").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.dictionary.confirm_discard = true;
                            this.dictionary.return_after_discard = false;
                            cx.notify();
                        }),
                    ),
                ),
        )
    }
    pub(super) fn dictionary_panel(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let available = self.dictionary.available();
        let no_draft = self.dictionary.editor.as_ref().is_none_or(|e| e.frozen);
        let revision = self
            .dictionary
            .page
            .as_ref()
            .map(|p| p.revision.clone())
            .unwrap_or_default();
        let mut body = div()
            .id("user-dictionary-body")
            .track_scroll(&self.dictionary.scroll)
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4()
            .px_6()
            .pb_6()
            .child(control::scroll_begin())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button(
                            "dictionary-add-entry",
                            "単語を登録…",
                            "単語の登録画面を開く",
                        )
                        .inactive(!available || !no_draft)
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let Some(p) = &this.dictionary.page {
                                this.edit_dictionary_draft(Draft::new(p, None), window, cx);
                            }
                        })),
                    )
                    .child(
                        button("dictionary-add", "辞書を作成…", "辞書の作成画面を開く")
                            .inactive(!available || !no_draft)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(p) = &this.dictionary.page {
                                    this.edit_dictionary_draft(
                                        Draft::dictionary(p, None),
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        button(
                            "dictionary-refresh",
                            "一覧・処理状況を更新",
                            "辞書一覧と処理状況を更新する",
                        )
                        .inactive(self.dictionary.pending)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.load_dictionary(this.dictionary.offset, cx)
                        })),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("入力を確定し、操作が落ち着くと変更が反映されます。"),
            );
        if let Some(job) = &self.dictionary.job {
            body = body.child(card().p_3().gap_2().child(job.message()).when(
                !job.finished(),
                |v| {
                    v.child(
                        button(
                            "dictionary-cancel-job",
                            "保存前の変更をキャンセル",
                            "保存前の辞書変更をキャンセルする",
                        )
                        .inactive(self.dictionary.pending || job.saved_revision.is_some())
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_dictionary(cx))),
                    )
                },
            ));
        }
        if let Some(page) = &self.dictionary.page {
            body=body.child(section("辞書").child(div().id("dictionary-names").max_h(px(140.)).overflow_y_scroll().flex().flex_col().gap_2().children(page.dictionaries.iter().map(|d|{
                let info=d.clone(); let deletion=Edit::DeleteDictionary{revision:revision.clone(),dictionary_index:d.index};
                let message=format!("「{}」と、その中の{}件の単語を削除します。この操作は取り消せません。",d.name,d.entries);
                card().p_3().flex_row().flex_wrap().gap_2().items_center()
                    .child(div().flex_1().min_w_0().child(format!("{}（{}件）",d.name,d.entries)))
                    .child(button(("dictionary-rename",d.index as u64),"名前を変更…","辞書名を変更する").inactive(!available || !no_draft).on_click(cx.listener(move |this,_,window,cx| {if let Some(page)=&this.dictionary.page {this.edit_dictionary_draft(Draft::dictionary(page,Some(&info)),window,cx);}})))
                    .child(button(("dictionary-delete",d.index as u64),"削除…","辞書の削除を確認する").inactive(!available || !no_draft).on_click(cx.listener(move |this,_,_,cx|{if this.dictionary.available() && this.dictionary.editor.as_ref().is_none_or(|e|e.frozen) {this.dictionary.confirm_delete=Some((deletion.clone(),message.clone()));cx.notify();}})))
            }))));
            body = body.child(self.dictionary_editor(cx));
            body = body.child(
                section("登録した単語")
                    .child(format!(
                        "{}件中 {}〜{}件",
                        page.total,
                        if page.entries.is_empty() {
                            0
                        } else {
                            page.offset + 1
                        },
                        page.offset + page.entries.len() as u32
                    ))
                    .when(page.entries.is_empty(), |v| {
                        v.child("このページに登録済みの単語はありません。")
                    })
                    .children(page.entries.iter().map(|entry| {
                        let entry: Entry = entry.clone();
                        let editing = entry.clone();
                        let deletion = Edit::Delete {
                            revision: revision.clone(),
                            dictionary_index: entry.dictionary_index,
                            entry_index: entry.entry_index,
                        };
                        let message = format!(
                            "「{} → {}」を辞書から削除します。この操作は取り消せません。",
                            entry.reading, entry.value
                        );
                        let dict = page
                            .dictionaries
                            .iter()
                            .find(|d| d.index == entry.dictionary_index)
                            .map(|d| d.name.clone())
                            .unwrap_or_default();
                        card()
                            .p_3()
                            .gap_2()
                            .child(format!("{} → {}", entry.reading, entry.value))
                            .child(div().text_sm().text_color(rgb(MUTED)).child(format!(
                                "{} · {}",
                                dict,
                                entry.pos.as_deref().unwrap_or("不明")
                            )))
                            .when(!entry.comment.is_empty(), |v| {
                                v.child(div().text_sm().child(entry.comment.clone()))
                            })
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        button(
                                            format!(
                                                "entry-edit-{}-{}",
                                                entry.dictionary_index, entry.entry_index
                                            ),
                                            "編集…",
                                            "この単語を編集する",
                                        )
                                        .inactive(!available || !no_draft)
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                if let Some(p) = &this.dictionary.page {
                                                    this.edit_dictionary_draft(
                                                        Draft::new(p, Some(&editing)),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            }),
                                        ),
                                    )
                                    .child(
                                        button(
                                            format!(
                                                "entry-delete-{}-{}",
                                                entry.dictionary_index, entry.entry_index
                                            ),
                                            "削除…",
                                            "この単語の削除を確認する",
                                        )
                                        .inactive(!available || !no_draft)
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                if this.dictionary.available()
                                                    && this
                                                        .dictionary
                                                        .editor
                                                        .as_ref()
                                                        .is_none_or(|e| e.frozen)
                                                {
                                                    this.dictionary.confirm_delete =
                                                        Some((deletion.clone(), message.clone()));
                                                    cx.notify();
                                                }
                                            }),
                                        ),
                                    ),
                            )
                    })),
            );
            let previous = page.offset.saturating_sub(dictionary::PAGE_SIZE);
            let next = page.offset.saturating_add(dictionary::PAGE_SIZE);
            body = body.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button(
                            "dictionary-previous",
                            "前のページ",
                            "辞書の前のページを表示する",
                        )
                        .inactive(!available || !no_draft || page.offset == 0)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.dictionary.available()
                                && this.dictionary.editor.as_ref().is_none_or(|e| e.frozen)
                                && this.dictionary.offset > 0
                            {
                                this.load_dictionary(previous, cx);
                            }
                        })),
                    )
                    .child(
                        button(
                            "dictionary-next",
                            "次のページ",
                            "辞書の次のページを表示する",
                        )
                        .inactive(!available || !no_draft || next >= page.total)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.dictionary.available()
                                && this.dictionary.editor.as_ref().is_none_or(|e| e.frozen)
                                && this
                                    .dictionary
                                    .page
                                    .as_ref()
                                    .is_some_and(|p| next < p.total)
                            {
                                this.load_dictionary(next, cx);
                            }
                        })),
                    ),
            );
        }
        // Keep confirmations visible even when the list is scrolled away.
        body = body.child(control::scroll_end(self.dictionary.scroll.clone()));
        let mut confirmation = div().flex_none().px_6().gap_2().flex().flex_col();
        if let Some((edit, message)) = &self.dictionary.confirm_delete {
            let edit = edit.clone();
            confirmation = confirmation.child(
                card()
                    .p_3()
                    .bg(rgb(0xffecee))
                    .gap_2()
                    .child(message.clone())
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                button(
                                    "dictionary-confirm-delete",
                                    "削除する",
                                    "確認した辞書の内容を削除する",
                                )
                                .inactive(!available || edit.revision() != revision)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.start_dictionary(edit.clone(), window, cx)
                                    },
                                )),
                            )
                            .child(
                                button(
                                    "dictionary-cancel-delete",
                                    "キャンセル",
                                    "削除をキャンセルする",
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.dictionary.confirm_delete = None;
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            );
        }
        if self.dictionary.confirm_discard {
            confirmation=confirmation.child(card().p_3().gap_2().child("保存していない編集内容を破棄しますか？送信済みの処理は、この操作では取り消しません。")
                .child(div().flex().gap_2()
                    .child(button("dictionary-confirm-discard","編集を破棄","未保存の編集内容を破棄する").on_click(cx.listener(|this,_,window,cx|{this.dictionary.editor=None;this.dictionary.confirm_discard=false;this.dictionary.proposal=None;if this.dictionary.return_after_discard {this.dictionary_open=false;}this.dictionary.return_after_discard=false;window.blur(cx);if this.dictionary.close_after_discard {cx.quit();}else{cx.notify();}})))
                    .child(button("dictionary-keep-draft","編集を続ける","編集に戻る").on_click(cx.listener(|this,_,_,cx|{this.dictionary.confirm_discard=false;this.dictionary.return_after_discard=false;this.dictionary.close_after_discard=false;cx.notify();})))));
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
                            .child(app_heading("ユーザー辞書", window))
                            .child(
                                button("dictionary-back", "設定へ戻る", "辞書画面から設定へ戻る")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this
                                            .dictionary
                                            .editor
                                            .as_ref()
                                            .is_some_and(|e| !e.frozen)
                                        {
                                            this.dictionary.confirm_discard = true;
                                            this.dictionary.return_after_discard = true;
                                            this.dictionary.close_after_discard = false;
                                        } else {
                                            this.dictionary_open = false;
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(ACCENT))
                            .child(self.dictionary.message.clone()),
                    ),
            )
            .child(confirmation)
            .child(body)
    }
}
