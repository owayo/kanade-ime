use anyhow::Result;
use gpui::{
    App, Bounds, Context, QuitMode, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};
use kanade_engine::config::Config;
#[cfg(target_os = "macos")]
use kanade_ipc::settings_log::{
    Component as LogComponent, Event as LogEvent, record as settings_log,
};
use kanade_ipc::{Manage, StoreState, manage::Action};
mod control;
mod dictionary_panel;
mod icon;
mod learning_panel;
#[cfg(target_os = "macos")]
mod macos;
mod model_panel;
mod project_panel;
mod project_readings;
mod project_workflow;
mod text_field;
const WINDOW_SIZE: gpui::Size<gpui::Pixels> = size(px(680.0), px(700.0));
const WINDOW_MIN_SIZE: gpui::Size<gpui::Pixels> = size(px(560.0), px(420.0));
struct Settings {
    learning_editor: Option<learning_panel::Editor>,
    learning_busy: bool,
    learning_epoch: u64,
    navigation_focus: gpui::FocusHandle,
    scroll: gpui::ScrollHandle,
    project_scroll: gpui::ScrollHandle,
    models: Option<crate::models::Status>,
    model_busy: bool,
    model_selection: crate::models::ModelSelection,
    model_remove_confirm: bool,
    config: Config,
    settings_loaded: bool,
    /// What the engine could read, as of the last refresh.
    stores: crate::Stores,
    loading: bool,
    records: Vec<serde_json::Value>,
    total: u64,
    offset: u32,
    status: String,
    pending_clear: Option<Action>,
    projects_open: bool,
    projects: kanade_indexer::management::Listing,
    projects_loaded: bool,
    project_pending: bool,
    project_monitor: Option<kanade_indexer::monitor::Report>,
    project_monitor_polling: bool,
    pending_project_revoke: Option<kanade_indexer::management::Edit>,
    project_scope: project_panel::ScopePanel,
    dictionary_open: bool,
    dictionary: dictionary_panel::DictionaryPanel,
}
impl Settings {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            learning_editor: None,
            learning_busy: false,
            learning_epoch: 0,
            navigation_focus: cx.focus_handle(),
            scroll: gpui::ScrollHandle::new(),
            project_scroll: gpui::ScrollHandle::new(),
            models: None,
            model_busy: false,
            model_selection: Default::default(),
            model_remove_confirm: false,
            config: Config::default(),
            settings_loaded: false,
            stores: crate::Stores::UNKNOWN,
            loading: false,
            records: vec![],
            total: 0,
            offset: 0,
            status: String::new(),
            pending_clear: None,
            projects_open: false,
            projects: Default::default(),
            projects_loaded: false,
            project_pending: false,
            project_monitor: None,
            project_monitor_polling: false,
            pending_project_revoke: None,
            project_scope: Default::default(),
            dictionary_open: false,
            dictionary: Default::default(),
        }
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.learning_busy {
            return;
        }
        self.loading = true;
        self.status = "設定を取得しています。".into();
        let offset = self.offset;
        let task = cx.background_spawn(async move {
            let config = crate::settings_with_stores();
            let records = crate::manage(Manage {
                action: Action::ListLearning as i32,
                offset,
                ..Default::default()
            });
            let models = crate::models::status();
            (config, records, models)
        });
        cx.spawn(async move |view, cx| {
            let (config, records, models) = task.await;
            let _ = view.update(cx, |this, cx| {
                this.loading = false;
                this.models = models.ok();
                this.model_selection.refresh(this.models.as_ref());
                if this
                    .models
                    .as_ref()
                    .is_some_and(crate::models::Status::busy)
                    && !this.model_busy
                {
                    this.monitor_models(cx);
                }
                #[cfg(target_os = "macos")]
                if config.is_err() || records.is_err() {
                    settings_log(LogComponent::Settings, LogEvent::LoadFailed);
                } else {
                    settings_log(LogComponent::Settings, LogEvent::Loaded);
                }
                let config_ok = config.is_ok();
                if let Ok((config, stores)) = config {
                    this.config = config;
                    this.stores = stores;
                    this.settings_loaded = true;
                }
                this.status = match records {
                    Ok(value) => {
                        this.records = value["records"].as_array().cloned().unwrap_or_default();
                        this.total = value["total"].as_u64().unwrap_or(0);
                        if config_ok {
                            ""
                        } else {
                            "設定を取得できませんでした。接続を確認して「更新」を押してください。"
                        }
                    }
                    Err(_) => {
                        "エンジンに接続できませんでした。接続を確認して「更新」を押してください。"
                    }
                }
                .into();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn update(&mut self, change: impl FnOnce(&mut Config)) {
        if self.loading || self.learning_busy {
            return;
        }
        if !self.settings_loaded {
            // Unloaded defaults must not overwrite the saved settings.
            self.status =
                "設定を取得できていないため変更できません。「更新」を押してください。".into();
            return;
        }
        if self.stores.settings == StoreState::Unreadable {
            // The engine refuses too: the file it could not read stays as it is.
            self.status = "設定ファイルを読めないため、変更を保存できません。".into();
            return;
        }
        let before = self.config.clone();
        change(&mut self.config);
        match crate::save(&self.config) {
            Ok(()) => {
                self.status = if before.learning != self.config.learning
                    || before.learning_excluded_apps != self.config.learning_excluded_apps
                {
                    "保存しました。学習設定の変更後は入力ソースを切り替えてください。"
                } else {
                    "保存しました。"
                }
                .into()
            }
            Err(_) => {
                self.config = before;
                self.status = "保存できませんでした。エンジンの接続を確認してください。".into();
            }
        }
    }
    fn clear_mozc(&mut self) {
        let Some(action) = self.pending_clear.take() else {
            return;
        };
        self.status = match crate::manage(Manage {
            action: action as i32,
            ..Default::default()
        }) {
            Ok(value) if value["cleared"] == true => {
                "Mozc の学習を消去しました。Kanade の訂正記録は保持しています。"
            }
            Ok(value) if value["reason"] == "composition_in_progress" => {
                "未確定入力を終えてから、もう一度消去してください。"
            }
            _ => "消去を確認できませんでした。エンジンの接続を確認してください。",
        }
        .into();
    }
}
const PAGE: u32 = 0xf6f7fa;
const CARD: u32 = 0xffffff;
const INK: u32 = 0x202938;
const MUTED: u32 = 0x657080;
const ACCENT: u32 = 0x355d9a;
const DANGER: u32 = 0xa43a46;
const LINE: u32 = 0xd9dde5;
const HOVER: u32 = 0xeef1f6;
fn app_heading(title: &'static str, window: &Window) -> gpui::Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_3()
        .child(
            gpui::img(icon::for_scale(window.scale_factor()))
                .size(px(icon::POINTS))
                .flex_none(),
        )
        .child(
            div()
                .text_2xl()
                .font_weight(gpui::FontWeight::BOLD)
                .child(title),
        )
}
fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    aria: &'static str,
) -> control::Control {
    let id = id.into();
    control::Control::new(id.clone(), div().id(id))
        .role(gpui::Role::Button)
        .aria_label(aria)
        .flex_none()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(LINE))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(HOVER)))
        .child(label.into())
}
fn section(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_sm().text_color(rgb(MUTED)).child(title))
}
fn card() -> gpui::Div {
    div()
        .rounded_lg()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(LINE))
        .flex()
        .flex_col()
}
fn separator() -> gpui::Div {
    div().h(px(1.0)).bg(rgb(LINE))
}
fn caption(label: &'static str, detail: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_1()
        .min_w_0()
        .flex_col()
        .gap_1()
        .child(label)
        .child(div().text_sm().text_color(rgb(MUTED)).child(detail))
}
/// One end of a numeric setting: element ID, accessible label and the change.
type SettingStep = (&'static str, &'static str, fn(&mut Config));
impl Settings {
    /// Whether the settings can be changed now. Not while the engine cannot
    /// read its settings file, which it then never writes.
    fn ready(&self) -> bool {
        self.settings_loaded
            && !self.loading
            && !self.learning_busy
            && self.stores.settings != StoreState::Unreadable
    }
    /// Why the engine runs without its settings or its learning store, and
    /// what brings them back. Shown until a refresh finds them readable.
    fn store_notice(&self) -> Option<gpui::Div> {
        let settings = self.stores.settings == StoreState::Unreadable;
        let learning = self.stores.learning == StoreState::Unreadable;
        if !settings && !learning {
            return None;
        }
        // The engine names the file it opened; this application may run with
        // another data directory.
        let file = |named: &str, name: &str| {
            if named.is_empty() {
                name.to_string()
            } else {
                named.to_string()
            }
        };
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .bg(rgb(0xffecee))
                .when(settings, |notice| {
                    notice.child(format!(
                        "設定ファイル（{}）を読めないため、学習や Tsumugi などの機能を止めています。このファイルは書き換えません。直すか削除してから「更新」を押すと、読み直します。",
                        file(&self.stores.settings_file, "settings.toml")
                    ))
                })
                .when(learning, |notice| {
                    notice.child(format!(
                        "学習の記録（{}）を読めないため、Kanade の学習を止めています。このファイルは書き換えません。直すか削除してから{}し直すと、読み直します。",
                        file(&self.stores.learning_file, "learning.json"),
                        if cfg!(windows) { "サインイン" } else { "ログイン" }
                    ))
                }),
        )
    }
    fn toggle(
        &self,
        id: &'static str,
        label: &'static str,
        detail: &'static str,
        on: bool,
        change: fn(&mut Config),
        cx: &mut Context<Self>,
    ) -> control::Control {
        // Defaults are shown until the engine answers; do not present them as
        // the saved state.
        let (state, active) = if !self.settings_loaded {
            ("未取得", false)
        } else if on {
            ("オン", true)
        } else {
            ("オフ", false)
        };
        control::Control::new(id, div().id(id))
            .role(gpui::Role::Switch)
            .aria_label(label)
            .aria_toggled(if active {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .p_3()
            .cursor_pointer()
            .hover(|style| style.bg(rgb(HOVER)))
            .when(!self.ready(), |row| row.opacity(0.5))
            .inactive(!self.ready())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update(change);
                cx.notify();
            }))
            .child(caption(label, detail))
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .text_sm()
                    .when(active, |pill| pill.bg(rgb(ACCENT)).text_color(rgb(CARD)))
                    .when(!active, |pill| pill.bg(rgb(HOVER)).text_color(rgb(MUTED)))
                    .child(state),
            )
    }
    fn stepper(
        &self,
        label: &'static str,
        detail: &'static str,
        value: String,
        smaller: SettingStep,
        larger: SettingStep,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .p_3()
            .when(!self.ready(), |row| row.opacity(0.5))
            .child(caption(label, detail))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .child(
                        button(smaller.0, "−", smaller.1)
                            .inactive(!self.ready())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.update(smaller.2);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .w(px(44.0))
                            .text_center()
                            .child(if self.settings_loaded {
                                value
                            } else {
                                "—".into()
                            }),
                    )
                    .child(
                        button(larger.0, "＋", larger.1)
                            .inactive(!self.ready())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.update(larger.2);
                                cx.notify();
                            })),
                    ),
            )
    }
    fn records_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let last = (self.offset as u64 + 50).min(self.total);
        section("候補・訂正の記録")
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(if self.stores.learning == StoreState::Unreadable {
                        "記録を読めないため、表示できません。".to_string()
                    } else if self.total == 0 {
                        "記録はありません。".to_string()
                    } else {
                        format!("{} 件中 {}〜{} 件", self.total, self.offset + 1, last)
                    })
                    .child(
                        button(
                            "refresh",
                            if self.loading {
                                "取得中…"
                            } else {
                                "更新"
                            },
                            "設定と訂正記録を更新する",
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.refresh(cx);
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(self.records.iter().filter_map(|r| {
                        let id = r["id"].as_u64()?;
                        let reading = r["correction"]["reading"].as_str().unwrap_or("");
                        let accepted = r["correction"]["accepted"].as_str().unwrap_or("");
                        let count = r["selection_count"].as_u64().unwrap_or(0);
                        let proposal_reading = reading.to_string();
                        let proposal_value = accepted.to_string();
                        Some(
                            card()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .gap_4()
                                .p_3()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(format!("{reading} → {accepted}"))
                                        .child(div().text_sm().text_color(rgb(MUTED)).child(
                                            if count > 0 {
                                                format!("候補から選択：{count}回")
                                            } else {
                                                "確定後の訂正".into()
                                            },
                                        )),
                                )
                                .child(
                                    button(
                                        ("register-correction", id),
                                        "辞書へ登録…",
                                        "この学習した語をユーザー辞書に登録する",
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.register_correction(
                                                proposal_reading.clone(),
                                                proposal_value.clone(),
                                                window,
                                                cx,
                                            );
                                        },
                                    )),
                                )
                                .child(
                                    button(("delete", id), "削除", "この訂正記録を削除する")
                                        .text_sm()
                                        .text_color(rgb(DANGER))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            match crate::manage(Manage {
                                                action: Action::DeleteLearning as i32,
                                                record_id: id,
                                                ..Default::default()
                                            }) {
                                                Ok(_) => {
                                                    this.status =
                                                        "選好と語彙の記録を削除しました。".into();
                                                    this.refresh(cx);
                                                }
                                                Err(_) => {
                                                    this.status = "削除できませんでした。".into()
                                                }
                                            }
                                            cx.notify();
                                        })),
                                ),
                        )
                    })),
            )
            .when(self.total > 50, |section| {
                section.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            button("previous", "前の50件", "前の50件").on_click(cx.listener(
                                |this, _, _, cx| {
                                    if !this.loading {
                                        this.offset = this.offset.saturating_sub(50);
                                        this.refresh(cx);
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                        .child(button("next", "次の50件", "次の50件").on_click(cx.listener(
                            |this, _, _, cx| {
                                if !this.loading && (this.offset as u64 + 50) < this.total {
                                    this.offset += 50;
                                    this.refresh(cx);
                                }
                                cx.notify();
                            },
                        ))),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("「削除」は Kanade の記録だけを削除します。Mozc の学習は変わりません。"),
            )
    }
    fn mozc_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        section("Mozc の学習")
            .child(
                div().flex().gap_2().children(
                    [
                        (
                            "clear-conversion",
                            "Mozc の変換学習を消去…",
                            Action::ClearMozcConversion,
                        ),
                        (
                            "clear-prediction",
                            "Mozc の予測学習を消去…",
                            Action::ClearMozcPrediction,
                        ),
                    ]
                    .into_iter()
                    .map(|(id, label, action)| {
                        button(id, label, label).on_click(cx.listener(move |this, _, _, cx| {
                            this.pending_clear = Some(action);
                            this.status.clear();
                            cx.notify();
                        }))
                    }),
                ),
            )
            .children(self.pending_clear.map(|action| {
                let scope = if action == Action::ClearMozcConversion {
                    "変換"
                } else {
                    "予測"
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(0xffecee))
                    .child(format!(
                        "Mozc の{scope}学習を消去します。この操作は取り消せません。"
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                button(
                                    "confirm-clear",
                                    "消去する",
                                    "選択した Mozc の学習を消去する",
                                )
                                .text_color(rgb(DANGER))
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.clear_mozc();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                button("cancel-clear", "キャンセル", "学習の消去をキャンセルする")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.pending_clear = None;
                                        cx.notify();
                                    })),
                            ),
                    )
            }))
    }
}
impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dictionary_open {
            return self
                .dictionary_panel(window, cx)
                .track_focus(&self.navigation_focus)
                .into_any_element();
        }
        if self.projects_open {
            return self
                .project_panel(window, cx)
                .track_focus(&self.navigation_focus)
                .into_any_element();
        }
        div()
            .size_full()
            .track_focus(&self.navigation_focus)
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
                // The title and the result of the last operation stay in view
                // while the settings below scroll.
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_6()
                    .pt_6()
                    .pb_3()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(app_heading("Kanade の設定", window))
                            .child(
                                button("open-user-dictionary", "ユーザー辞書を管理…", "ユーザー辞書を管理する")
                                    .inactive(self.learning_editor.is_some())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.dictionary_open = true;
                                        this.load_dictionary(0, cx);
                                    })),
                            )
                            .child(
                                button(
                                    "open-projects",
                                    "プロジェクト辞書を管理…",
                                    "プロジェクト辞書を管理する",
                                )
                                .inactive(self.learning_editor.is_some())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.projects_open = true;
                                    this.load_projects(cx);
                                })),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child("通常の入力は端末内で処理します。パスワード欄では学習しません。"),
                    )
                    .child(
                        div()
                            .min_h(px(20.0))
                            .text_sm()
                            .text_color(rgb(ACCENT))
                            .child(self.status.clone()),
                    )
                    .when_some(self.store_notice(), |header, notice| header.child(notice))
                    .when_some(self.learning_close_notice(cx), |header, notice| header.child(notice)),
            )
            .child(
                div()
                    .id("settings-body")
                    .track_scroll(&self.scroll)
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .px_6()
                    .pb_6()
                    .child(control::scroll_begin())
                    .child(
                        section("入力").child(
                            card()
                                .child(self.toggle(
                                    "learning",
                                    "候補・訂正学習",
                                    "選んで確定した候補と、確定後の訂正を記録し、次の候補順に反映します。",
                                    self.config.learning,
                                    |c| c.learning = !c.learning,
                                    cx,
                                ))
                                .child(separator())
                                .child(self.toggle(
                                    "punctuation-commit",
                                    "句読点で確定",
                                    "変換前の読みに「。」「、」「？」「！」を続けて入力すると、そのまま確定します。",
                                    self.config.punctuation_commit,
                                    |c| c.punctuation_commit = !c.punctuation_commit,
                                    cx,
                                ))
                                .child(separator())
                                .child(self.toggle(
                                    "calculated-dates",
                                    "計算日付",
                                    "「こんげつまつ」などの読みに、計算した日付の候補を追加します。",
                                    self.config.calculated_dates,
                                    |c| c.calculated_dates = !c.calculated_dates,
                                    cx,
                                ))
                                .child(separator())
                                .child(self.toggle(
                                    "emoji-dictionary",
                                    "絵文字辞書",
                                    "読みに一致する絵文字の候補を追加します。",
                                    self.config.emoji_dictionary,
                                    |c| c.emoji_dictionary = !c.emoji_dictionary,
                                    cx,
                                )),
                        ),
                    )
                    .child(
                        section("候補").child(
                            card()
                                .child(self.stepper(
                                    "候補の文字サイズ",
                                    "12〜40 の範囲で変更できます。",
                                    self.config.font_size.to_string(),
                                    ("smaller", "候補の文字を小さくする", |c| {
                                        c.font_size = (c.font_size - 1.0).max(12.0)
                                    }),
                                    ("larger", "候補の文字を大きくする", |c| {
                                        c.font_size = (c.font_size + 1.0).min(40.0)
                                    }),
                                    cx,
                                ))
                                .child(separator())
                                .child(self.stepper(
                                    "1 ページの候補数",
                                    "数字キーで選べる候補の数です。1〜9 の範囲で変更できます。",
                                    self.config.page_size.to_string(),
                                    ("fewer-candidates", "1 ページの候補を減らす", |c| {
                                        c.page_size = c.page_size.saturating_sub(1).max(1)
                                    }),
                                    ("more-candidates", "1 ページの候補を増やす", |c| {
                                        c.page_size = (c.page_size + 1).min(9)
                                    }),
                                    cx,
                                )),
                        ),
                    )
                    .child(self.learning_section(cx))
                    .child(self.model_section(cx))
                    .child(self.records_section(cx))
                    .child(self.mozc_section(cx))
                    .child(control::scroll_end(self.scroll.clone())),
            )
            .into_any_element()
    }
}
pub fn run() -> Result<()> {
    #[cfg(target_os = "macos")]
    settings_log(LogComponent::Settings, LogEvent::Started);
    let application = gpui_platform::application().with_quit_mode(QuitMode::LastWindowClosed);
    // On macOS the Apple event handler installed below receives reopen
    // requests first; this remains for a request that still reaches GPUI.
    application.on_reopen(|cx| {
        #[cfg(target_os = "macos")]
        settings_log(LogComponent::Settings, LogEvent::Reopened);
        show_settings(cx);
    });
    application.run(|cx| {
        text_field::init(cx);
        control::init(cx);
        #[cfg(target_os = "macos")]
        {
            install_commands(cx);
            // GPUI reports a reopen request only while no window is visible, so
            // a window on another Space or display would never be shown again.
            let reopen = cx.to_async();
            macos::on_reopen(move || {
                // Deferred: the event can arrive while GPUI is in an update.
                reopen
                    .spawn(async move |cx| {
                        cx.update(|cx| {
                            settings_log(LogComponent::Settings, LogEvent::Reopened);
                            show_settings(cx);
                        });
                    })
                    .detach();
            });
        }
        show_settings(cx);
    });
    Ok(())
}
/// Answers an explicit request for the settings window: the first launch and
/// every later request reuse one window and bring it in front of the user.
fn show_settings(cx: &mut App) {
    let window = if let Some(window) = cx.windows().first().copied() {
        window
    } else {
        // Open on the display in use, not unconditionally on the primary one.
        #[cfg(target_os = "macos")]
        let display = macos::pointer_display().map(|id| gpui::DisplayId::new(id.into()));
        #[cfg(not(target_os = "macos"))]
        let display = None;
        let bounds = Bounds::centered(display, WINDOW_SIZE, cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id: display,
                window_min_size: Some(WINDOW_MIN_SIZE),
                titlebar: Some(TitlebarOptions {
                    title: Some("Kanade の設定".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    let view = cx.weak_entity();
                    window.on_window_should_close(cx, move |_, cx| {
                        view.update(cx, |this: &mut Settings, cx| {
                            this.confirm_settings_close(cx)
                        })
                        .unwrap_or(true)
                    });
                    #[cfg(target_os = "macos")]
                    cx.observe_window_activation(window, |_, window, _| {
                        settings_log(
                            LogComponent::Settings,
                            if window.is_window_active() {
                                LogEvent::WindowActivated
                            } else {
                                LogEvent::WindowDeactivated
                            },
                        );
                    })
                    .detach();
                    #[cfg(not(target_os = "macos"))]
                    let _ = window;
                    let mut state = Settings::new(cx);
                    window.focus(&state.navigation_focus, cx);
                    state.refresh(cx);
                    state
                })
            },
        );
        let Ok(window) = opened else {
            #[cfg(target_os = "macos")]
            settings_log(LogComponent::Settings, LogEvent::WindowFailed);
            eprintln!("kanade-ui: settings window could not open");
            cx.quit();
            return;
        };
        window.into()
    };
    cx.activate(true);
    #[cfg(target_os = "macos")]
    {
        settings_log(LogComponent::Settings, LogEvent::WindowRequested);
        present(window, cx);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = window.update(cx, |_, window, _| window.activate_window());
}
/// Restores, repositions and orders the window front, then records whether it
/// became visible. Only predefined lifecycle events are written.
#[cfg(target_os = "macos")]
fn present(window: gpui::AnyWindowHandle, cx: &mut App) {
    cx.spawn(async move |cx| {
        let Some(native) = window
            .update(cx, |_, window, _| macos::native_window(window))
            .ok()
            .flatten()
        else {
            return;
        };
        // The update above has ended: AppKit reports frame and key changes to
        // GPUI synchronously, which must not happen inside a GPUI update.
        let presented = macos::present(&native);
        if presented.restored {
            settings_log(LogComponent::Settings, LogEvent::WindowRestored);
        }
        if presented.relocated {
            settings_log(LogComponent::Settings, LogEvent::WindowRelocated);
        }
        // Occlusion and Space membership settle after the window server has
        // processed the ordering request.
        cx.background_executor()
            .timer(std::time::Duration::from_millis(600))
            .await;
        if window.update(cx, |_, _, _| ()).is_err() {
            return;
        }
        settings_log(
            LogComponent::Settings,
            match macos::visibility(&native) {
                macos::Visibility::Visible => LogEvent::WindowVisible,
                macos::Visibility::Occluded => LogEvent::WindowOccluded,
                macos::Visibility::OffSpace => LogEvent::WindowOffSpace,
            },
        );
    })
    .detach();
}
#[cfg(target_os = "macos")]
gpui::actions!(kanade_settings, [CloseWindow, MinimizeWindow, Quit]);
/// Standard window commands. Without an application menu, a GPUI application
/// has no Close, Minimize or Quit shortcut.
#[cfg(target_os = "macos")]
fn install_commands(cx: &mut App) {
    use gpui::{KeyBinding, Menu, MenuItem};
    cx.on_action(|_: &Quit, cx| {
        // Actions run inside the focused window's update. Read its root only
        // after that update ends, or the nested update would fail.
        cx.defer(|cx| request_settings_close(true, cx));
    });
    cx.on_action(|_: &CloseWindow, cx| {
        cx.defer(|cx| request_settings_close(false, cx));
    });
    cx.on_action(|_: &MinimizeWindow, cx| {
        if let Some(window) = cx.windows().first().copied() {
            let _ = window.update(cx, |_, window, _| window.minimize_window());
        }
    });
    cx.bind_keys([
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    cx.set_menus([
        Menu::new("Kanade の設定").items([MenuItem::action("Kanade の設定を終了", Quit)]),
        Menu::new("ウインドウ").items([
            MenuItem::action("閉じる", CloseWindow),
            MenuItem::action("しまう", MinimizeWindow),
        ]),
    ]);
}
#[cfg(target_os = "macos")]
fn request_settings_close(quit: bool, cx: &mut App) {
    let Some(window) = cx.windows().first().copied() else {
        if quit {
            cx.quit();
        }
        return;
    };
    let allowed = window.downcast::<Settings>().is_none_or(|settings| {
        settings
            .update(cx, |this, _, cx| this.confirm_settings_close(cx))
            .unwrap_or(false)
    });
    if allowed {
        if quit {
            cx.quit();
        } else {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    }
}
struct Review {
    findings: Vec<crate::writing::Finding>,
}
impl Render for Review {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .font_family(if cfg!(target_os = "macos") {
                "Hiragino Sans"
            } else {
                "Yu Gothic UI"
            })
            .id("writing-review")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p_6()
            .gap_4()
            .bg(rgb(0xf6f7fa))
            .text_color(rgb(0x202938))
            .child(div().text_2xl().child("文章の確認"))
            .child("指摘は変換や訂正学習に反映しません。元の文書で修正してください。")
            .child(format!("{} 件の指摘", self.findings.len()))
            .children(self.findings.iter().map(|f| {
                div()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(0xffffff))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(format!(
                        "{}  文字位置 {}〜{}",
                        f.rule, f.start_utf16, f.end_utf16
                    ))
                    .child(f.message.clone())
                    .child(f.hint.clone().unwrap_or_default())
            }))
    }
}
pub fn review_window(findings: Vec<crate::writing::Finding>) -> Result<()> {
    gpui_platform::application()
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx: &mut App| {
            if cx
                .open_window(WindowOptions::default(), |_, cx| {
                    cx.new(|_| Review { findings })
                })
                .is_err()
            {
                cx.quit();
                return;
            }
            cx.activate(true);
        });
    Ok(())
}
