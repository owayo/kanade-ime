use super::*;
use std::sync::{
    Arc, RwLock,
    mpsc::{self, SyncSender},
};
enum Job {
    Manage(AdminAction, SyncSender<io::Result<()>>),
    Learned(u64, Vec<Correction>, Vec<Word>, u64),
    Undo(u64),
    Flush(SyncSender<io::Result<()>>),
}
#[derive(Clone)]
enum Change {
    Correction(u64),
    Selection(SelectionUndo),
}
fn publish(shared: &RwLock<Store>, store: &Store) {
    let next = store.clone();
    if let Ok(mut current) = shared.write() {
        let previous = std::mem::replace(&mut *current, next);
        drop(current);
        drop(previous);
    }
}
#[derive(Clone)]
pub struct LearningService {
    worker: Arc<Worker>,
}
struct Worker {
    tx: Option<SyncSender<Job>>,
    snapshot: Arc<RwLock<Store>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl LearningService {
    pub fn start(path: PathBuf, dict: PathBuf) -> io::Result<Self> {
        let store = Store::load(&path)?;
        let snapshot = Arc::new(RwLock::new(store.clone()));
        let shared = snapshot.clone();
        let (tx, rx) = mpsc::sync_channel::<Job>(64);
        let thread = std::thread::Builder::new()
            .name("kanade-learning".into())
            .spawn(move || {
                let mut store = store;
                let mut analyzer = None;
                let mut source_ids = BTreeMap::<u64, Vec<Change>>::new();
                while let Ok(job) = rx.recv() {
                    let previous = store.clone();
                    let previous_sources = source_ids.clone();
                    match job {
                        Job::Manage(action, reply) => {
                            let before = store.clone();
                            let mut removed = std::collections::BTreeSet::new();
                            match action {
                                AdminAction::Delete(id) => {
                                    removed.insert(id);
                                    store.undo(id);
                                }
                                AdminAction::Clear => {
                                    store.clear();
                                    source_ids.clear();
                                }
                                AdminAction::Block(reading) => {
                                    let key = reading_key(&reading);
                                    removed.extend(
                                        store
                                            .records()
                                            .filter(|r| r.correction.reading == key)
                                            .map(|r| r.id),
                                    );
                                    store.block_reading(&reading);
                                }
                            }
                            source_ids.retain(|_, changes| {
                                changes.retain(|change| {
                                    !removed.contains(&match change {
                                        Change::Correction(id) => *id,
                                        Change::Selection(undo) => undo.id,
                                    })
                                });
                                !changes.is_empty()
                            });
                            let result = store.save(&path);
                            if result.is_ok() {
                                publish(&shared, &store);
                            } else {
                                store = before;
                                source_ids = previous_sources;
                            }
                            let _ = reply.send(result);
                            continue;
                        }
                        Job::Learned(source, corrections, selections, time) => {
                            let mut ids = vec![];
                            for word in selections {
                                if let Some(undo) = store.record_selection(word, time) {
                                    ids.push(Change::Selection(undo));
                                }
                            }
                            if !corrections.is_empty() && analyzer.is_none() {
                                analyzer = Analyzer::load(&dict).ok();
                            }
                            if let Some(ref mut analyzer) = analyzer {
                                for correction in corrections {
                                    if let Some(vocabulary) = classify(analyzer, &correction)
                                        && let Some(id) = store.apply(correction, time, vocabulary)
                                    {
                                        ids.push(Change::Correction(id));
                                    }
                                }
                            }
                            if !ids.is_empty() {
                                source_ids.insert(source, ids);
                                while source_ids.len() > 1024 {
                                    source_ids.pop_first();
                                }
                            }
                        }
                        Job::Undo(source) => {
                            if let Some(ids) = source_ids.remove(&source) {
                                for change in ids.into_iter().rev() {
                                    match change {
                                        Change::Correction(id) => {
                                            store.undo(id);
                                        }
                                        Change::Selection(undo) => {
                                            for changes in source_ids.values_mut() {
                                                for change in changes {
                                                    if let Change::Selection(later) = change {
                                                        later.forget(&undo);
                                                    }
                                                }
                                            }
                                            store.undo_selection(undo);
                                        }
                                    }
                                }
                            }
                        }
                        Job::Flush(reply) => {
                            let _ = reply.send(store.save(&path));
                            continue;
                        }
                    }
                    // Disk work and morphology never run in the engine's key thread.
                    if store.save(&path).is_ok() {
                        publish(&shared, &store);
                    } else {
                        store = previous;
                        source_ids = previous_sources;
                    }
                }
                let _ = store.save(&path);
            })?;
        Ok(Self {
            worker: Arc::new(Worker {
                tx: Some(tx),
                snapshot,
                thread: Some(thread),
            }),
        })
    }
    pub fn read<T>(&self, f: impl FnOnce(&Store) -> T) -> Option<T> {
        self.worker.snapshot.try_read().ok().map(|s| f(&s))
    }
    pub fn manage(&self, action: AdminAction) -> io::Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.worker
            .tx
            .as_ref()
            .ok_or(io::ErrorKind::BrokenPipe)?
            .try_send(Job::Manage(action, tx))
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| io::ErrorKind::TimedOut)?
    }
    pub fn corrections(&self, source: u64, corrections: Vec<Correction>, time: u64) -> bool {
        self.learned(source, corrections, vec![], time)
    }
    /// The engine queues only successful document acknowledgements. No input
    /// thread waits for morphology or persistence.
    pub fn learned(
        &self,
        source: u64,
        corrections: Vec<Correction>,
        selections: Vec<Word>,
        time: u64,
    ) -> bool {
        self.worker.tx.as_ref().is_some_and(|tx| {
            tx.try_send(Job::Learned(source, corrections, selections, time))
                .is_ok()
        })
    }
    pub fn undo(&self, source: u64) -> bool {
        self.worker
            .tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(Job::Undo(source)).is_ok())
    }
    pub fn flush(&self) -> io::Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.worker
            .tx
            .as_ref()
            .ok_or(io::ErrorKind::BrokenPipe)?
            .try_send(Job::Flush(tx))
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| io::ErrorKind::TimedOut)?
    }
}
pub enum AdminAction {
    Delete(u64),
    Clear,
    Block(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_failed_choice_save_does_not_publish_or_keep_the_increment() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("data");
        let path = folder.join("learning.json");
        let learning =
            LearningService::start(path.clone(), temp.path().join("absent.hsd")).unwrap();
        learning.learned(1, vec![], vec![term()], 0);
        learning.flush().unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
        std::fs::write(&folder, b"blocked directory").unwrap();
        learning.learned(2, vec![], vec![term()], 0);
        assert!(learning.flush().is_err());
        assert_eq!(
            learning.read(|s| s.records().next().unwrap().selection_count),
            Some(1)
        );
        std::fs::remove_file(&folder).unwrap();
        learning.flush().unwrap();
        assert_eq!(
            Store::load(&path)
                .unwrap()
                .records()
                .next()
                .unwrap()
                .selection_count,
            1
        );
    }
    fn term() -> Word {
        Word {
            reading: "こうせい".into(),
            value: "校正".into(),
        }
    }
    #[test]
    fn choices_need_no_dictionary_and_out_of_order_undo_does_not_resurrect_them() {
        for order in [[1, 2, 3], [3, 1, 2], [2, 3, 1]] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("learning.json");
            let mut base = Store::default();
            for _ in 0..5 {
                base.record_selection(term(), 0).unwrap();
            }
            base.save(&path).unwrap();
            let learning =
                LearningService::start(path.clone(), temp.path().join("absent.hsd")).unwrap();
            for source in 1..=3 {
                assert!(learning.learned(source, vec![], vec![term()], 0));
            }
            learning.flush().unwrap();
            assert_eq!(
                learning.read(|s| s.records().next().unwrap().selection_count),
                Some(8)
            );
            for source in order {
                assert!(learning.undo(source));
            }
            learning.flush().unwrap();
            assert_eq!(
                learning.read(|s| s.records().next().unwrap().selection_count),
                Some(5)
            );
            assert_eq!(
                learning.read(|s| s.records().next().unwrap().last_selected),
                Some(5)
            );
            assert_eq!(
                Store::load(&path)
                    .unwrap()
                    .records()
                    .next()
                    .unwrap()
                    .selection_count,
                5
            );
        }
    }
    #[test]
    fn administration_does_not_allow_an_old_receipt_to_restore_erased_learning() {
        let temp = tempfile::tempdir().unwrap();
        let learning = LearningService::start(
            temp.path().join("learning.json"),
            temp.path().join("absent.hsd"),
        )
        .unwrap();
        for source in 1..=2 {
            assert!(learning.learned(source, vec![], vec![term()], 0));
        }
        learning.manage(AdminAction::Clear).unwrap();
        learning.undo(2);
        learning.undo(1);
        learning.flush().unwrap();
        assert_eq!(learning.read(|s| s.records().count()), Some(0));
        learning.learned(3, vec![], vec![term()], 0);
        learning
            .manage(AdminAction::Block("こうせい".into()))
            .unwrap();
        learning.undo(3);
        learning.learned(4, vec![], vec![term()], 0);
        learning.flush().unwrap();
        assert_eq!(learning.read(|s| s.records().count()), Some(0));
    }
    #[test]
    fn failed_save_retains_the_published_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("data");
        let path = folder.join("learning.json");
        let mut store = Store::default();
        store
            .apply(
                Correction {
                    reading: "こうせい".into(),
                    accepted: "校正".into(),
                    rejected: Some("構成".into()),
                    evidence: Evidence::Strong,
                },
                0,
                true,
            )
            .unwrap();
        store.save(&path).unwrap();
        let learning = LearningService::start(path, temp.path().join("absent.hsd")).unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
        std::fs::write(&folder, b"blocked directory").unwrap();
        assert!(learning.manage(AdminAction::Clear).is_err());
        assert_eq!(learning.read(|s| s.records().count()), Some(1));
        assert_eq!(
            learning.read(|s| s.preferred("こうせい", &["校正".into()])),
            Some(Some("校正".into()))
        );
        drop(learning);
    }
}
