use super::*;
use crate::models::{self, CatalogEntry, Status};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

pub(super) fn bundled_model() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    #[cfg(windows)]
    let resources = exe.parent()?.to_owned();
    #[cfg(target_os = "macos")]
    let resources = exe.parent()?.parent()?.join("Resources");
    let path = resources.join("models/tsumugi.gguf");
    path.is_file().then_some(path)
}

impl Settings {
    pub(super) fn monitor_models(&mut self, cx: &mut Context<Self>) {
        if self.model_busy {
            return;
        }
        self.model_busy = true;
        self.model_notice = None;
        cx.spawn(async move |view, cx| {
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                let result = cx
                    .background_executor()
                    .spawn(async { models::status() })
                    .await;
                let keep = view.update(cx, |this, cx| {
                    this.models = result.ok();
                    this.model_busy = this.models.as_ref().is_some_and(Status::busy);
                    cx.notify();
                    this.model_busy
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = view.update(cx, |this, cx| {
                        this.model_busy = false;
                        this.model_notice =
                            Some("モデルの準備が続いています。「状態を更新」で確認できます。");
                        cx.notify();
                    });
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
            }
        })
        .detach();
        cx.notify();
    }

    fn model_operation(
        &mut self,
        operation: impl FnOnce() -> Result<serde_json::Value> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.model_busy {
            return;
        }
        self.model_busy = true;
        self.model_remove_confirm = false;
        self.model_notice = None;
        let task = cx.background_spawn(async move {
            let result = operation();
            let status = models::status().ok();
            (result, status)
        });
        cx.spawn(async move |view, cx| {
            let (result, status) = task.await;
            let _ = view.update(cx, |this, cx| {
                this.model_busy = false;
                this.models = status;
                if this.models.as_ref().is_some_and(Status::busy) {
                    this.monitor_models(cx);
                } else {
                    this.model_notice = match result.as_ref() {
                        Ok(value)
                            if value["saved"] == true
                                || matches!(value["result"].as_str(), Some("started" | "removed" | "not_installed")) => None,
                        Ok(value) if value["result"] == "consent_required" => {
                            Some("利用条件が変更されました。最新の内容を確認してください。")
                        }
                        Ok(value) if value["result"] == "failed" => {
                            Some("モデルの削除を完了できませんでした。ファイルへのアクセスを確認してください。")
                        }
                        _ => Some("操作の完了を確認できませんでした。表示されている状態を確認してください。"),
                    };
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_model(&mut self, model: CatalogEntry, cx: &mut Context<Self>) {
        if self.model_busy || !self.ready() {
            return;
        }
        if let Some(path) = self.bundled_model.clone() {
            self.model_operation(move || models::import_and_activate(&path, &model), cx);
            return;
        }
        self.model_busy = true;
        self.model_notice = None;
        let chooser = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("配布元から取得したGGUFファイルを選択".into()),
        });
        cx.spawn(async move |view, cx| {
            let choice = chooser.await;
            let _ = view.update(cx, |this, cx| {
                this.model_busy = false;
                match choice {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.model_operation(
                                move || models::import_and_activate(&path, &model),
                                cx,
                            );
                        }
                    }
                    Ok(Ok(None)) => {}
                    _ => {
                        this.model_notice =
                            Some("ファイルを選択できませんでした。もう一度お試しください。")
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn model_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let status = self.models.as_ref();
        let busy = self.model_busy || status.is_some_and(Status::busy);
        let editable = !busy && self.ready();
        let label = match status {
            _ if busy => "準備中",
            None => "状態未取得",
            Some(state) if state.ready && state.enabled => "オン",
            Some(state) if state.ready => "オフ",
            Some(state) if state.installed.is_some() => "要確認",
            _ => "未設定",
        };
        let active = !busy && status.is_some_and(|state| state.ready && state.enabled);
        let message = status.map(Status::message).unwrap_or(if self.model_busy {
            "モデルの状態を確認しています。"
        } else {
            "モデルの状態を取得できませんでした。"
        });
        let mut body = card().p_4().gap_3()
            .child(div().flex().items_center().justify_between().gap_3()
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("AI変換"))
                .child(div().flex_none().rounded_full().px_3().py_1().text_sm()
                    .bg(rgb(if active { ACCENT } else { HOVER }))
                    .text_color(rgb(if active { CARD } else { MUTED }))
                    .child(label)))
            .child(div().text_sm().text_color(rgb(MUTED)).child(
                "入力の文脈に合わせて変換候補を調整します。学習を許可したアプリで動作し、入力内容は端末内で処理します。",
            ))
            .child(div().text_sm().child(message))
            .when_some(self.model_notice, |view, notice| view.child(
                div().text_sm().text_color(rgb(DANGER)).child(notice),
            ));
        if busy {
            if status
                .and_then(|status| status.job.as_ref())
                .is_some_and(|job| !matches!(job.phase.as_str(), "done" | "failed"))
            {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("設定ウインドウを閉じても準備は続きます。"),
                );
            }
            if let Some(job) = status.and_then(|status| status.job.as_ref())
                && job.phase == "copying"
            {
                body = body.child(div().text_sm().child(format!("取り込み {}%", job.copied)));
            }
            if !self.model_busy {
                body = body.child(self.model_refresh_button(cx));
            }
            return body;
        }
        let Some(state) = status else {
            return body.child(self.model_refresh_button(cx));
        };
        if state.ready {
            let label = if state.enabled {
                "AI変換をオフ"
            } else {
                "AI変換をオン"
            };
            let enable = !state.enabled;
            body = body.child(
                button("llm-enabled", label, label)
                    .inactive(!editable)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.model_operation(move || models::set_enabled(enable), cx);
                    })),
            );
            if state.enabled && self.settings_loaded && !self.config.learning {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("学習がオフの間は、通常の変換候補が表示されます。"),
                );
            }
        } else if state.runtime == "available" {
            if state.needs_import() {
                if let Some(model) = state.catalog_entry() {
                    body =
                        body.child(self.model_setup(model, state.has_consent(model), editable, cx));
                }
            } else {
                body = body.child(
                    button(
                        "qualify-model",
                        "再検証して使う",
                        "モデルを再検証してAI変換をオンにする",
                    )
                    .inactive(!editable)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model_operation(models::qualify_and_activate, cx);
                    })),
                );
            }
        }
        if !state.ready && state.enabled {
            body = body.child(
                button("llm-enabled", "AI変換をオフ", "AI変換をオフにする")
                    .inactive(!editable)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model_operation(|| models::set_enabled(false), cx);
                    })),
            );
        }
        if let Some(installed) = &state.installed {
            body = body.child(separator()).child(
                div().flex().items_center().justify_between().gap_2().flex_wrap()
                    .child(div().text_sm().text_color(rgb(MUTED)).child(if installed.consent_current {
                        format!("{} · 利用条件に同意済み", installed.label())
                    } else {
                        installed.label()
                    }))
                    .child(button("model-details", if self.model_remove_confirm { "閉じる" } else { "モデルの管理…" }, "取り込んだモデルを管理する")
                        .text_sm()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model_remove_confirm = !this.model_remove_confirm;
                            cx.notify();
                        }))),
            ).when(self.model_remove_confirm, |view| view.child(
                div().flex().flex_col().gap_2().p_3().rounded_md().bg(rgb(HOVER))
                    .child(div().text_sm().child("モデルを削除するとAI変換がオフになり、同意と検証の記録も消去されます。取り込み元のファイルは保持します。"))
                    .child(div().flex().gap_2().flex_wrap()
                        .child(self.model_refresh_button(cx))
                        .child(button("confirm-remove-model", "モデルを削除する", "取り込んだモデルと同意記録を削除する")
                            .text_color(rgb(DANGER))
                            .inactive(!editable)
                            .on_click(cx.listener(|this, _, _, cx| this.model_operation(models::remove, cx))))),
            ));
        }
        if self.model_notice.is_some() && !self.model_remove_confirm {
            body = body.child(self.model_refresh_button(cx));
        }
        body
    }

    fn model_setup(
        &self,
        model: &CatalogEntry,
        consent: bool,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let bundled = self.bundled_model.is_some();
        let source = model.source_url.clone();
        let license = model.license_url.clone();
        let title = match (consent, bundled) {
            (true, true) => "モデルを準備する",
            (true, false) => "モデルファイルを選択…",
            (false, true) => "同意して使う",
            (false, false) => "同意してファイルを選ぶ…",
        };
        let model_choice = model.clone();
        div().flex().flex_col().gap_3().p_3().rounded_md().bg(rgb(HOVER))
            .child(div().text_sm().child(format!(
                "{}（約 {} MB） · {}",
                model.name, model.size.div_ceil(1_000_000), model.attribution,
            )))
            .when(!consent, |view| view.child(div().text_sm().child(
                "ライセンスと、カーソル直前の最大40文字・読み・変換候補を端末内で処理することへの同意が必要です。AI処理に使う内容の送信・保存・ログ記録は行いません。"
            )))
            .child(div().text_sm().text_color(rgb(MUTED)).child(if bundled {
                "同梱モデルを検証し、完了すると自動でオンになります。"
            } else {
                "配布元から GGUF ファイルを入手してください。選んだファイルは検証後に自動で使えるようになります。"
            }))
            .child(div().flex().flex_wrap().gap_2()
                .child(button("model-license", format!("ライセンス（{}）", model.license), "モデルのライセンスを開く")
                    .on_click(move |_, _, cx| cx.open_url(&license)))
                .child(button("model-source", "配布元", "モデルの配布元を開く")
                    .on_click(move |_, _, cx| cx.open_url(&source))))
            .child(div().flex().child(
                button("enable-model", title, title)
                    .when(!consent, |button| button.aria_description(
                        "モデルのライセンスと、入力の文脈を端末内で処理することに同意します。",
                    ))
                    .bg(rgb(ACCENT)).border_color(rgb(ACCENT)).text_color(rgb(CARD))
                    .hover(|style| style.bg(rgb(0x294c82)))
                    .when(!editable, |button| button.opacity(0.55))
                    .inactive(!editable)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_model(model_choice.clone(), cx);
                    })),
            ))
    }

    fn model_refresh_button(&self, cx: &mut Context<Self>) -> control::Control {
        button("refresh-model", "状態を更新", "AI変換の状態を更新する")
            .inactive(self.model_busy)
            .on_click(cx.listener(|this, _, _, cx| this.monitor_models(cx)))
    }
}
