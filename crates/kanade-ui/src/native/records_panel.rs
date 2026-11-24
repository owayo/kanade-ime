use super::*;

const PAGE_SIZE: u32 = 50;

#[derive(Default)]
pub(super) struct Records {
    entries: Vec<serde_json::Value>,
    total: u64,
    offset: u32,
    loaded: bool,
    unreadable: bool,
    loading: bool,
    notice: Option<&'static str>,
}

fn fetch(offset: u32) -> Result<(serde_json::Value, crate::Stores)> {
    crate::manage_with_stores(Manage {
        action: Action::ListLearning as i32,
        offset,
        ..Default::default()
    })
}

impl Settings {
    fn load_records(&mut self, offset: u32, delete: Option<u64>, cx: &mut Context<Self>) {
        if self.records.loading {
            return;
        }
        self.records.loading = true;
        self.records.notice = None;
        let task = cx.background_spawn(async move {
            let deletion_failed = delete.is_some_and(|record_id| {
                crate::manage(Manage {
                    action: Action::DeleteLearning as i32,
                    record_id,
                    ..Default::default()
                })
                .is_err()
            });
            let result = fetch(offset).and_then(|(value, stores)| {
                let total = value["total"].as_u64().unwrap_or(0);
                if offset > 0
                    && u64::from(offset) >= total
                    && stores.learning != StoreState::Unreadable
                {
                    let size = u64::from(PAGE_SIZE);
                    return fetch(((total.saturating_sub(1) / size) * size) as u32);
                }
                Ok((value, stores))
            });
            (result, deletion_failed)
        });
        cx.spawn(async move |view, cx| {
            let (result, deletion_failed) = task.await;
            let _ = view.update(cx, |this, cx| {
                let records = &mut this.records;
                records.loading = false;
                match result {
                    Ok((value, stores)) => {
                        records.loaded = true;
                        records.unreadable = stores.learning == StoreState::Unreadable;
                        records.entries = value["records"].as_array().cloned().unwrap_or_default();
                        records.total = value["total"].as_u64().unwrap_or(0);
                        records.offset = value["offset"].as_u64().unwrap_or(0) as u32;
                        records.notice = if deletion_failed {
                            Some("削除の完了を確認できませんでした。更新した一覧で確認してください。")
                        } else if delete.is_some() {
                            Some("学習記録を削除しました。")
                        } else {
                            None
                        };
                        if this.page == SettingsPage::Records {
                            this.scroll.set_offset(gpui::point(px(0.), px(0.)));
                        }
                    }
                    Err(_) => {
                        records.notice = Some("学習記録を取得できませんでした。「更新」を押してください。");
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn reload_records(&mut self, cx: &mut Context<Self>) {
        self.load_records(self.records.offset, None, cx);
    }

    pub(super) fn records_toolbar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let records = &self.records;
        let offset = records.offset;
        let end = (u64::from(offset) + u64::from(PAGE_SIZE)).min(records.total);
        let summary = if records.loading {
            "取得中…".into()
        } else if records.unreadable {
            "学習記録を読み込めません".into()
        } else if !records.loaded {
            "学習記録は未取得です".into()
        } else if records.total == 0 {
            "学習記録はありません".into()
        } else {
            format!(
                "{} 件中 {}〜{} 件",
                records.total,
                u64::from(offset) + 1,
                end
            )
        };
        let refresh = button("refresh-records", "更新", "学習記録を更新する")
            .inactive(records.loading)
            .on_click(cx.listener(|this, _, _, cx| this.reload_records(cx)));
        let mut actions = div().flex().gap_2();
        if offset > 0 || end < records.total {
            actions = actions
                .child(
                    button("previous-records", "前へ", "学習記録の前のページ")
                        .inactive(records.loading || offset == 0)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.load_records(offset.saturating_sub(PAGE_SIZE), None, cx);
                        })),
                )
                .child(
                    button("next-records", "次へ", "学習記録の次のページ")
                        .inactive(records.loading || end >= records.total)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.load_records(offset.saturating_add(PAGE_SIZE), None, cx);
                        })),
                );
        }
        div()
            .flex()
            .flex_none()
            .flex_col()
            .gap_2()
            .px_6()
            .pb_3()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().text_sm().child(summary))
                    .child(actions.child(refresh)),
            )
            .when_some(records.notice, |toolbar, notice| {
                toolbar.child(div().text_sm().text_color(rgb(MUTED)).child(notice))
            })
    }

    pub(super) fn records_list(&self, cx: &mut Context<Self>) -> gpui::Div {
        let loading = self.records.loading;
        let offset = self.records.offset;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(self.records.entries.iter().filter_map(|record| {
                let id = record["id"].as_u64()?;
                let reading = record["correction"]["reading"].as_str().unwrap_or("");
                let word = record["correction"]["accepted"].as_str().unwrap_or("");
                let count = record["selection_count"].as_u64().unwrap_or(0);
                let proposal_reading = reading.to_string();
                let proposal_word = word.to_string();
                Some(
                    card()
                        .flex_none()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .p_3()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(format!("{reading} → {word}"))
                                .child(div().text_sm().text_color(rgb(MUTED)).child(
                                    if count > 0 {
                                        format!("候補から選択 {count} 回")
                                    } else {
                                        "確定後の訂正".into()
                                    },
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .gap_2()
                                .child(
                                    button(
                                        ("register-correction", id),
                                        "辞書へ登録…",
                                        "この学習した語をユーザー辞書に登録する",
                                    )
                                    .inactive(loading)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.register_correction(
                                                proposal_reading.clone(),
                                                proposal_word.clone(),
                                                cx,
                                            );
                                        },
                                    )),
                                )
                                .child(
                                    button(("delete-record", id), "削除", "この学習記録を削除する")
                                        .text_color(rgb(DANGER))
                                        .inactive(loading)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.load_records(offset, Some(id), cx);
                                        })),
                                ),
                        ),
                )
            }))
    }
}
