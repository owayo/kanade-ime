//! Typed project controls shared by the settings view and process-level tests.
use anyhow::Result;
use kanade_indexer::management::{
    AppScope, Edit, Listing, Operation, Reply, ScopeQuery, valid_apps,
};
pub use kanade_indexer::workflow::{
    ApprovalRequest, Preview, Request as JobRequest, Stage, Status as Job,
};
use kanade_ipc::{Manage, manage::Action};

pub fn monitoring() -> Result<kanade_indexer::monitor::Report> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::ProjectMonitorStatus as i32,
        ..Default::default()
    })?)?)
}
pub fn readings(
    query: kanade_indexer::overrides::Query,
) -> Result<kanade_indexer::overrides::Page> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::ListProjectReadings as i32,
        json: serde_json::to_string(&query)?,
        ..Default::default()
    })?)?)
}
pub fn prepare(request: ApprovalRequest) -> Result<Preview> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::PrepareProject as i32,
        json: serde_json::to_string(&request)?,
        ..Default::default()
    })?)?)
}
pub fn start_job(request: JobRequest) -> Result<Job> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::StartProjectJob as i32,
        json: serde_json::to_string(&request)?,
        ..Default::default()
    })?)?)
}
pub fn jobs(id: u64) -> Result<Vec<Job>> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::ProjectJobStatus as i32,
        job_id: id,
        ..Default::default()
    })?)?)
}
pub fn cancel_job(id: u64) -> Result<Job> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::CancelProjectJob as i32,
        job_id: id,
        ..Default::default()
    })?)?)
}
pub fn job_message(job: &Job) -> &'static str {
    match job.state {
        Stage::Queued | Stage::Preparing => "プロジェクト辞書を準備しています。",
        Stage::Indexing => "フォルダを解析して辞書を作成しています。入力はそのまま続けられます。",
        Stage::Reloading => "辞書を作成しました。入力エンジンに反映しています。",
        Stage::Ready if job.reloaded => "辞書を作成して入力エンジンに反映しました。",
        Stage::Ready => {
            "辞書は作成済みですが、入力エンジンへの反映を確認できませんでした。一覧を更新してください。"
        }
        Stage::Cancelled if job.saved_revision.is_some() => {
            "辞書作成をキャンセルしました。保存済みのプロジェクト設定は保持しています。"
        }
        Stage::Cancelled => "辞書作成をキャンセルしました。既存の辞書は保持しています。",
        Stage::Failed if job.saved_revision.is_some() => {
            "プロジェクト設定は保存済みですが、辞書を作成できませんでした。一覧の「辞書を再作成」からやり直せます。"
        }
        Stage::Failed => {
            "辞書を作成できませんでした。フォルダや承認の変更を確認してください。既存の辞書は保持しています。"
        }
    }
}
pub fn list() -> Result<Listing> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::ListProjects as i32,
        ..Default::default()
    })?)?)
}
pub fn edit(edit: Edit) -> Result<Reply> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::EditProject as i32,
        json: serde_json::to_string(&edit)?,
        ..Default::default()
    })?)?)
}

pub fn scope(query: ScopeQuery) -> Result<AppScope> {
    Ok(serde_json::from_value(crate::manage(Manage {
        action: Action::ListProjects as i32,
        json: serde_json::to_string(&query)?,
        ..Default::default()
    })?)?)
}

#[derive(Clone)]
pub struct ScopeDraft {
    pub original: AppScope,
    pub apps: Vec<String>,
}
impl ScopeDraft {
    pub fn new(scope: AppScope) -> Self {
        Self {
            apps: scope.apps.clone(),
            original: scope,
        }
    }
    pub fn edit(&self, listing: &Listing) -> std::result::Result<Edit, &'static str> {
        if listing.revision != self.original.revision
            || !listing.projects.iter().any(|project| {
                project.id == self.original.id && project.approval == self.original.approval
            })
        {
            return Err("承認や設定が変わりました。一覧から編集を開き直してください。");
        }
        if !valid_apps(&self.apps) {
            return Err(
                "対象アプリを1〜64件指定してください。各IDは512バイト以内で、空欄や改行は使えません。",
            );
        }
        Ok(Edit {
            revision: self.original.revision,
            id: self.original.id.clone(),
            approval: self.original.approval,
            operation: Operation::SetApps {
                apps: self.apps.clone(),
            },
        })
    }
    pub fn matches(&self, current: &AppScope) -> bool {
        current.id == self.original.id
            && current.approval == self.original.approval
            && current.root == self.original.root
            && current.apps == self.apps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_indexer::management::Summary;
    #[test]
    fn scope_draft_cannot_rebind_to_a_new_revision_or_approval() {
        let original = AppScope {
            revision: 4,
            id: "sample".into(),
            approval: 7,
            root: "source".into(),
            apps: vec!["com.example.Editor".into()],
        };
        let mut draft = ScopeDraft::new(original);
        draft.apps = vec!["com.example.Terminal".into()];
        let mut listing = Listing {
            revision: 4,
            projects: vec![Summary {
                id: "sample".into(),
                approval: 7,
                root: "source".into(),
                enabled: true,
                apps: 1,
                readings: 0,
                monitoring: false,
            }],
        };
        assert!(draft.edit(&listing).is_ok());
        listing.revision += 1;
        assert!(draft.edit(&listing).is_err());
        listing.revision = 4;
        listing.projects[0].approval += 1;
        assert!(draft.edit(&listing).is_err());
    }
    #[test]
    fn unknown_write_requires_exact_readback_in_the_same_approval() {
        let original = AppScope {
            revision: 4,
            id: "sample".into(),
            approval: 7,
            root: "source".into(),
            apps: vec!["com.example.Editor".into()],
        };
        let mut draft = ScopeDraft::new(original.clone());
        draft.apps = vec!["com.example.Terminal".into()];
        assert!(!draft.matches(&original));
        let mut observed = original;
        observed.revision += 1;
        observed.apps = draft.apps.clone();
        assert!(draft.matches(&observed));
        observed.approval += 1;
        assert!(!draft.matches(&observed));
    }
}
