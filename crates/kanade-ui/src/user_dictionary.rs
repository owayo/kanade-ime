//! Explicit dictionary management. Drafts are memory-only and never retried.
use anyhow::{Result, bail};
use kanade_ipc::{Manage, manage::Action};
pub use kanade_mozc::dictionary::{DictionaryInfo, Edit, Entry, NewEntry, Page};
use serde::Deserialize;

pub const PAGE_SIZE: u32 = 50;
pub const PARTS_OF_SPEECH: &[(&str, &str)] = &[
    ("noun", "名詞"),
    ("proper_noun", "固有名詞"),
    ("personal_name", "人名"),
    ("place_name", "地名"),
    ("organization_name", "組織名"),
    ("adverb", "副詞"),
    ("interjection", "感動詞"),
    ("symbol", "記号"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Entry {
        dictionary: Option<u32>,
        entry: Option<u32>,
    },
    CreateDictionary,
    RenameDictionary(u32),
}
#[derive(Clone, Debug)]
pub struct Draft {
    pub revision: String,
    pub target: Target,
    pub reading: String,
    pub value: String,
    pub pos: String,
    pub comment: String,
    pub name: String,
}
impl Draft {
    pub fn new(page: &Page, entry: Option<&Entry>) -> Self {
        Self {
            revision: page.revision.clone(),
            target: Target::Entry {
                dictionary: entry.map(|e| e.dictionary_index),
                entry: entry.map(|e| e.entry_index),
            },
            reading: entry.map(|e| e.reading.clone()).unwrap_or_default(),
            value: entry.map(|e| e.value.clone()).unwrap_or_default(),
            pos: entry
                .and_then(|e| e.pos.clone())
                .unwrap_or_else(|| "noun".into()),
            comment: entry.map(|e| e.comment.clone()).unwrap_or_default(),
            name: String::new(),
        }
    }
    pub fn dictionary(page: &Page, existing: Option<&DictionaryInfo>) -> Self {
        Self {
            target: existing
                .map(|d| Target::RenameDictionary(d.index))
                .unwrap_or(Target::CreateDictionary),
            name: existing.map(|d| d.name.clone()).unwrap_or_default(),
            ..Self::new(page, None)
        }
    }
    pub fn validate(&self, page: &Page) -> Result<(), &'static str> {
        if self.revision != page.revision {
            return Err("辞書が変更されました。一覧を更新して編集を開き直してください。");
        }
        fn text(value: &str, required: bool) -> bool {
            (!required || !value.trim().is_empty())
                && value.len() <= 300
                && !value.chars().any(char::is_control)
        }
        match self.target {
            Target::Entry { dictionary, entry } => {
                if dictionary.is_some_and(|i| !page.dictionaries.iter().any(|d| d.index == i))
                    || (entry.is_some() && dictionary.is_none())
                {
                    return Err("保存先が変わりました。一覧から編集を開き直してください。");
                }
                if !text(&self.reading, true)
                    || !text(&self.value, true)
                    || !text(&self.comment, false)
                {
                    return Err(
                        "読みと単語を入力してください。各欄は300バイト以内で、改行は使えません。",
                    );
                }
                if !self.reading.chars().all(|c| c.is_ascii_graphic()
                    || matches!(c, 'ぁ'..='ゖ' | '゙'..='゜' | 'ァ'..='ヺ' | '・' | 'ー' | '、' | '。' | '「'..='』' | '〜' | '！'..='～' | '｡'..='ﾟ')) {
                    return Err("読みには、かな・英数字・対応する記号を入力してください。");
                }
                if self.pos.is_empty() {
                    return Err("品詞を選択してください。");
                }
            }
            Target::CreateDictionary | Target::RenameDictionary(_) if !text(&self.name, true) => {
                return Err("辞書名を300バイト以内で入力してください。改行は使えません。");
            }
            Target::RenameDictionary(i) if !page.dictionaries.iter().any(|d| d.index == i) => {
                return Err("辞書が見つかりません。一覧を更新してください。");
            }
            _ => {}
        }
        Ok(())
    }
    pub fn edit(&self, page: &Page) -> Result<Edit> {
        if let Err(message) = self.validate(page) {
            bail!(message);
        }
        let revision = self.revision.clone();
        Ok(match self.target {
            Target::Entry { dictionary, entry } => Edit::Upsert {
                revision,
                dictionary_index: dictionary,
                entry_index: entry,
                entry: NewEntry {
                    reading: self.reading.clone(),
                    value: self.value.clone(),
                    pos: self.pos.clone(),
                    comment: self.comment.clone(),
                },
            },
            Target::CreateDictionary => Edit::CreateDictionary {
                revision,
                name: self.name.clone(),
            },
            Target::RenameDictionary(dictionary_index) => Edit::RenameDictionary {
                revision,
                dictionary_index,
                name: self.name.clone(),
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Queued,
    Preparing,
    WaitingForIdle,
    Loading,
    Ready,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Job {
    pub id: String,
    pub state: Stage,
    pub saved_revision: Option<String>,
    pub reason: Option<String>,
}
impl Job {
    pub fn finished(&self) -> bool {
        matches!(self.state, Stage::Ready | Stage::Failed | Stage::Cancelled)
    }
    pub fn message(&self) -> &'static str {
        match self.state {
            Stage::Queued | Stage::Preparing => "変更を準備しています。",
            Stage::WaitingForIdle => "入力の確定後、操作が落ち着いてから保存します。",
            Stage::Loading => "保存しました。入力エンジンに反映しています。",
            Stage::Ready => "保存して入力エンジンに反映しました。",
            Stage::Cancelled => "変更をキャンセルしました。",
            Stage::Failed if self.saved_revision.is_some() => {
                "保存済みですが、入力エンジンへの反映を確認できませんでした。再登録せず、状態を更新してください。"
            }
            Stage::Failed if self.reason.as_deref() == Some("revision_conflict") => {
                "辞書が変更されていたため保存しませんでした。一覧から編集を開き直してください。"
            }
            Stage::Failed => "変更を保存できませんでした。一覧を更新して状態を確認してください。",
        }
    }
    fn request_id(&self) -> Result<u64> {
        let id = self.id.parse::<u64>()?;
        if id == 0 {
            bail!("invalid dictionary job identity");
        }
        Ok(id)
    }
}
pub fn list(offset: u32) -> Result<Page> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::ListMozcDictionary as i32,
        offset,
        ..Default::default()
    })?)?)
}
/// An active job always takes precedence over an earlier completed request.
pub fn observed_job(current: Option<&Job>, jobs: &[Job]) -> Option<Job> {
    jobs.iter()
        .find(|job| !job.finished())
        .or_else(|| current.and_then(|current| jobs.iter().find(|job| job.id == current.id)))
        .cloned()
}
pub fn start(edit: Edit) -> Result<Job> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::EditMozcDictionary as i32,
        json: serde_json::to_string(&edit)?,
        ..Default::default()
    })?)?)
}
pub fn jobs() -> Result<Vec<Job>> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::DictionaryJobStatus as i32,
        ..Default::default()
    })?)?)
}
pub fn poll(job: &Job) -> Result<Job> {
    let value = crate::manage(Manage {
        action: Action::DictionaryJobStatus as i32,
        job_id: job.request_id()?,
        ..Default::default()
    })?;
    let jobs: Vec<Job> = serde_json::from_value(value)?;
    jobs.into_iter()
        .find(|found| found.id == job.id)
        .ok_or_else(|| anyhow::anyhow!("dictionary job unavailable"))
}
pub fn cancel(job: &Job) -> Result<Job> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::CancelDictionaryJob as i32,
        job_id: job.request_id()?,
        ..Default::default()
    })?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page() -> Page {
        Page {
            revision: "first".into(),
            dictionaries: vec![DictionaryInfo {
                index: 3,
                name: "Notes".into(),
                entries: 1,
            }],
            entries: vec![],
            offset: 0,
            total: 0,
        }
    }
    #[test]
    fn editing_binds_revision_indices_and_preserves_unlisted_part_of_speech() {
        let page = page();
        let entry = Entry {
            dictionary_index: 3,
            entry_index: 8,
            reading: "たんご".into(),
            value: "単語".into(),
            pos: Some("suggestion_only".into()),
            comment: "note".into(),
        };
        let draft = Draft::new(&page, Some(&entry));
        let Edit::Upsert {
            revision,
            dictionary_index,
            entry_index,
            entry,
        } = draft.edit(&page).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (revision.as_str(), dictionary_index, entry_index),
            ("first", Some(3), Some(8))
        );
        assert_eq!(entry.pos, "suggestion_only");
        let mut changed = page.clone();
        changed.revision = "second".into();
        assert!(draft.edit(&changed).is_err());
        changed.revision = "first".into();
        changed.dictionaries.clear();
        assert!(draft.edit(&changed).is_err());
    }
    #[test]
    fn manual_registration_accepts_kana_and_refuses_invalid_or_oversize_fields() {
        let page = page();
        let mut draft = Draft::new(&page, None);
        draft.reading = "かな".into();
        draft.value = "仮名".into();
        assert!(draft.edit(&page).is_ok());
        for bad in ["漢字", "a b", "a\n", ""] {
            draft.reading = bad.into();
            assert!(draft.edit(&page).is_err());
        }
        draft.reading = "かな".into();
        draft.value = "字".repeat(101);
        assert!(draft.edit(&page).is_err());
        draft.value = "字".repeat(100);
        assert!(draft.edit(&page).is_ok());
        draft.comment = "\0".into();
        assert!(draft.edit(&page).is_err());
    }
    #[test]
    fn dictionary_names_are_explicit_revision_bound_edits() {
        let page = page();
        let mut draft = Draft::dictionary(&page, None);
        assert!(draft.edit(&page).is_err());
        draft.name = "Work".into();
        assert!(matches!(
            draft.edit(&page).unwrap(),
            Edit::CreateDictionary { .. }
        ));
        let mut rename = Draft::dictionary(&page, Some(&page.dictionaries[0]));
        rename.name = "Home".into();
        assert!(matches!(
            rename.edit(&page).unwrap(),
            Edit::RenameDictionary {
                dictionary_index: 3,
                ..
            }
        ));
    }
    #[test]
    fn saved_but_not_reloaded_is_distinct_and_job_id_is_never_a_json_number() {
        let mut job:Job=serde_json::from_value(serde_json::json!({"id":"18446744073709551615","state":"failed","saved_revision":"new","reason":"reload_failed"})).unwrap();
        assert_eq!(job.request_id().unwrap(), u64::MAX);
        assert!(job.finished());
        assert!(job.message().starts_with("保存済み"));
        job.saved_revision = None;
        job.reason = Some("revision_conflict".into());
        assert!(job.message().contains("保存しませんでした"));
        job.id = "0".into();
        assert!(job.request_id().is_err());
    }
    #[test]
    fn refresh_tracks_other_active_edits_instead_of_an_older_completed_job() {
        let old = Job {
            id: "1".into(),
            state: Stage::Ready,
            saved_revision: Some("saved".into()),
            reason: None,
        };
        let active = Job {
            id: "2".into(),
            state: Stage::WaitingForIdle,
            saved_revision: None,
            reason: None,
        };
        assert_eq!(
            observed_job(Some(&old), &[old.clone(), active.clone()])
                .unwrap()
                .id,
            "2"
        );
        assert_eq!(
            observed_job(Some(&old), std::slice::from_ref(&old))
                .unwrap()
                .id,
            "1"
        );
        assert!(observed_job(Some(&active), &[]).is_none());
    }
}
