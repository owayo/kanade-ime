use super::*;
use std::time::{Duration, Instant};

impl Settings {
    pub(super) fn monitor_models(&mut self, cx: &mut Context<Self>) {
        if self.model_busy {
            return;
        }
        self.model_busy = true;
        cx.spawn(async move |view, cx| {
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                let result = cx
                    .background_executor()
                    .spawn(async { crate::models::status() })
                    .await;
                let keep = view.update(cx, |this, cx| {
                    let keep = match result {
                        Ok(state) => {
                            let busy = state.busy();
                            this.status = state.message().into();
                            this.config.llm = state.enabled;
                            this.model_selection.refresh(Some(&state));
                            this.models = Some(state);
                            busy
                        }
                        Err(_) => {
                            this.models = None;
                            this.model_selection.refresh(None);
                            this.status = "モデルの状態を取得できませんでした。「更新」で確認してください。".into();
                            false
                        }
                    };
                    this.model_busy = keep;
                    cx.notify();
                    keep
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = view.update(cx, |this, cx| {
                        this.model_busy = false;
                        this.status = "モデルの処理が続いています。「モデルの状態を更新」で進行を確認してください。".into();
                        cx.notify();
                    });
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(500)).await;
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
        self.status = "モデルの操作を開始しています。".into();
        let task = cx.background_spawn(async move { operation() });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                this.model_busy = false;
                let accepted = result.as_ref().is_ok_and(|v| {
                    matches!(v["result"].as_str(), Some("started" | "removed" | "not_installed"))
                });
                if accepted {
                    this.monitor_models(cx);
                } else {
                    this.status = "モデルの操作を開始できませんでした。同意内容・対応モデル・処理中の状態を確認してください。".into();
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_model(&mut self, cx: &mut Context<Self>) {
        if self.model_busy {
            return;
        }
        let Some(model) = self
            .models
            .as_ref()
            .and_then(|state| self.model_selection.selected(state))
            .cloned()
        else {
            return;
        };
        if !self.model_selection.accepted(&model) {
            self.status = "ライセンスと端末内での処理への同意を確認してください。".into();
            cx.notify();
            return;
        }
        let bundled = std::env::current_exe().ok().and_then(|exe| {
            #[cfg(windows)]
            let resources = exe.parent()?.to_owned();
            #[cfg(target_os = "macos")]
            let resources = exe.parent()?.parent()?.join("Resources");
            Some(resources.join("models/kanade-tsumugi-v0.1.0-s-F16.gguf"))
        });
        if let Some(path) = bundled.filter(|p| p.is_file()) {
            self.model_operation(move || crate::models::import(&path, &model, true, true), cx);
            return;
        }
        let chooser = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("取得したGGUFモデルを選択".into()),
        });
        cx.spawn(async move |view, cx| {
            if let Ok(Ok(Some(paths))) = chooser.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = view.update(cx, |this, cx| {
                    if this.model_selection.accepted(&model) {
                        this.model_operation(
                            move || crate::models::import(&path, &model, true, true),
                            cx,
                        );
                    }
                });
            }
        })
        .detach();
    }

    pub(super) fn model_section(&self, cx: &mut Context<Self>) -> gpui::Div {
        let ready = self.models.as_ref().is_some_and(|s| s.ready);
        let busy = self.model_busy
            || self
                .models
                .as_ref()
                .is_some_and(crate::models::Status::busy);
        let state = self
            .models
            .as_ref()
            .map(crate::models::Status::message)
            .unwrap_or("モデルの状態は未取得です。「モデルの状態を更新」で確認してください。");
        let selected = self
            .models
            .as_ref()
            .and_then(|status| self.model_selection.selected(status));
        let installed = self.models.as_ref().and_then(|status| {
            status.installed.as_ref().map(|installed| {
                status
                    .catalog
                    .iter()
                    .find(|model| model.id == installed.id)
                    .map(|model| model.name.clone())
                    .unwrap_or_else(|| installed.id.clone())
            })
        });
        section("ローカルAI").child(
            card().p_3().gap_3()
                .child("文脈に応じた候補の並べ替え")
                .child(div().text_sm().text_color(rgb(MUTED)).child(
                    "通常の変換はMozcで行います。小型モデルは端末内で候補を評価し、選択中の候補を保持したまま、ほかの候補の順番を調整します。学習を許可したアプリでのみ動作し、入力内容は外部へ送信しません。",
                ))
                .child(div().text_sm().text_color(rgb(ACCENT)).child(state))
                .when_some(installed, |view, name| view.child(div().text_sm().child(format!("導入済み: {name}"))))
                .child(button("llm-enabled", if self.config.llm { "並べ替えを無効にする" } else { "並べ替えを有効にする" }, "文脈による候補の並べ替えを切り替える")
                    .when(busy || (!ready && !self.config.llm), |v| v.opacity(0.5))
                    .inactive(busy || (!ready && !self.config.llm))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !busy && (ready || this.config.llm) {
                            this.update(|c| c.llm = !c.llm);
                            this.monitor_models(cx);
                        }
                        cx.notify();
                    })))
                .child(separator())
                .child("使用モデル: Tsumugi S")

                .when_some(selected, |view, model| {
                    let source = model.source_url.clone();
                    let license = model.license_url.clone();
                    view.child(separator())
                        .child(format!("{}（約{}MB）", model.name, model.size.div_ceil(1_000_000)))
                        .child(div().text_sm().child(format!("提供: {} / {}", model.attribution, model.license)))
                        .child(div().flex().flex_wrap().gap_2()
                            .child(button("model-source", "配布元を開く", "モデルの配布元を開く").on_click(move |_, _, cx| cx.open_url(&source)))
                            .child(button("model-license", "ライセンスを読む", "モデルのライセンスを開く").on_click(move |_, _, cx| cx.open_url(&license))))
                        .child(control::Control::new("model-license-consent", div().id("model-license-consent")).role(gpui::Role::Switch).aria_label("モデルのライセンスに同意する")
                            .aria_toggled(if self.model_selection.license { gpui::Toggled::True } else { gpui::Toggled::False })
                            .cursor_pointer().py_2()
                            .inactive(busy)
                            .child(format!("{} モデルのライセンスに同意します。", if self.model_selection.license { "☑" } else { "☐" }))
                            .on_click(cx.listener(|this, _, _, cx| { if !this.model_busy { this.model_selection.license = !this.model_selection.license; cx.notify(); } })))
                        .child(control::Control::new("model-processing-consent", div().id("model-processing-consent")).role(gpui::Role::Switch).aria_label("文脈と候補を端末内のモデルで処理することに同意する")
                            .aria_toggled(if self.model_selection.local_processing { gpui::Toggled::True } else { gpui::Toggled::False })
                            .cursor_pointer().py_2()
                            .inactive(busy)
                            .child(format!("{} カーソル直前の最大40文字・読み・候補を端末内のモデルで処理することに同意します。", if self.model_selection.local_processing { "☑" } else { "☐" }))
                            .on_click(cx.listener(|this, _, _, cx| { if !this.model_busy { this.model_selection.local_processing = !this.model_selection.local_processing; cx.notify(); } })))
                        .child(button("import-model", "Tsumugi を取り込む", "Tsumugi モデルを検証して取り込む")
                            .when(busy || !self.model_selection.accepted(model), |v| v.opacity(0.5))
                            .inactive(busy || !self.model_selection.accepted(model))
                            .on_click(cx.listener(|this, _, _, cx| this.choose_model(cx))))
                })
                .child(div().flex().flex_wrap().gap_2()
                    .child(button("refresh-model", "モデルの状態を更新", "モデルの状態を更新する")
                        .inactive(busy)
                        .on_click(cx.listener(|this, _, _, cx| this.monitor_models(cx))))
                    .child(button("qualify-model", "モデルを再検証", "導入したモデルを再検証する")
                        .when(busy, |v| v.opacity(0.5))
                        .inactive(busy || self.models.as_ref().is_none_or(|status| status.installed.is_none()))
                        .on_click(cx.listener(|this, _, _, cx| this.model_operation(crate::models::qualify, cx))))
                    .child(button("remove-model", "モデルを削除…", "導入したモデルの削除を確認する")
                        .when(busy, |v| v.opacity(0.5))
                        .inactive(busy || self.models.as_ref().is_none_or(|status| status.installed.is_none()))
                        .on_click(cx.listener(|this, _, _, cx| { if !this.model_busy { this.model_remove_confirm = true; cx.notify(); } }))))
                .when(self.model_remove_confirm, |v| v.child(
                    div().flex().flex_col().gap_2().p_3().bg(rgb(0xffecee))
                        .child("取り込んだモデルと検証・同意の記録を削除し、候補の並べ替えを無効にします。取り込み元のファイルは保持します。")
                        .child(div().flex().gap_2()
                            .child(button("confirm-remove-model", "削除する", "取り込んだモデルを削除する")
                                .inactive(busy)
                                .on_click(cx.listener(|this, _, _, cx| this.model_operation(crate::models::remove, cx))))
                            .child(button("cancel-remove-model", "キャンセル", "モデルの削除をキャンセルする")
                                .on_click(cx.listener(|this, _, _, cx| { this.model_remove_confirm = false; cx.notify(); }))))
                )),
        )
    }
}
