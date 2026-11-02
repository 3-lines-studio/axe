use crate::durable::Journal;
use crate::ids::SessionId;
use crate::session::{self, Entry, SessionMeta, SessionState, Store};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

const CURRENT: &str = "session.current";
const RESUME: &str = "session.resume_id";

pub struct JournalStore {
    dir: PathBuf,
    journal: Mutex<Option<Journal>>,
    current: Mutex<SessionId>,
}

impl JournalStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);
        let current_path = dir.join(CURRENT);
        let current = std::fs::read_to_string(&current_path)
            .ok()
            .and_then(|value| SessionId::new(value.trim()))
            .unwrap_or_else(|| {
                let id = SessionId::generate();
                let _ = crate::atomic_write(&current_path, id.as_str().as_bytes());
                id
            });
        Self {
            dir,
            journal: Mutex::new(None),
            current: Mutex::new(current),
        }
    }

    fn current_id(&self) -> SessionId {
        self.current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn set_current(&self, id: SessionId) {
        let _ = crate::atomic_write(&self.dir.join(CURRENT), id.as_str().as_bytes());
        *self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = id;
    }

    fn with_journal<T>(
        &self,
        run: impl FnOnce(&mut Journal) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let mut guard = self
            .journal
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if guard.is_none() {
            *guard = Some(Journal::open(&self.dir)?);
        }
        run(guard.as_mut().unwrap())
    }

    fn records(&self) -> Vec<session::Record> {
        if let Some(journal) = self
            .journal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            return journal.entries().to_vec();
        }
        Journal::load(&self.dir).unwrap_or_default()
    }

    fn records_for(&self, id: &SessionId) -> Vec<session::Record> {
        self.records()
            .into_iter()
            .filter(|record| &record.session == id)
            .collect()
    }

    fn write_current(&self) -> SessionId {
        let id = SessionId::generate();
        self.set_current(id.clone());
        id
    }

    fn resume_path(&self) -> PathBuf {
        self.dir.join(RESUME)
    }
}

impl Store for JournalStore {
    fn live(&self) -> Vec<Entry> {
        let id = self.current_id();
        self.records_for(&id)
            .into_iter()
            .map(|record| record.entry)
            .collect()
    }

    fn save(&self, entries: &[Entry]) -> std::io::Result<()> {
        self.save_with_state(entries, SessionState::default())
    }

    fn append(&self, entries: &[Entry]) -> std::io::Result<()> {
        let id = self.current_id();
        self.with_journal(|journal| {
            journal
                .commit_session(&id, entries.to_vec(), Vec::new())
                .map(|_| ())
        })
    }

    fn state(&self) -> SessionState {
        let id = self.current_id();
        if let Some(journal) = self
            .journal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            return journal.state(&id);
        }
        Journal::load_state(&self.dir, &id).unwrap_or_default()
    }

    fn append_with_state(&self, entries: &[Entry], mut state: SessionState) -> std::io::Result<()> {
        let id = self.current_id();
        if !entries.is_empty() {
            let index = self.live().len() + entries.len() - 1;
            state.point(index, state.context_tokens());
        }
        self.with_journal(|journal| {
            journal
                .commit_session_with_state(&id, entries.to_vec(), Vec::new(), Some(state))
                .map(|_| ())
        })
    }

    fn save_with_state(&self, entries: &[Entry], state: SessionState) -> std::io::Result<()> {
        let id = self.current_id();
        self.with_journal(|journal| journal.replace_session(&id, entries.to_vec(), state))
    }

    fn archive(&self) -> Option<String> {
        let old = self.current_id();
        self.write_current();
        Some(old.as_str().to_string())
    }

    fn continue_archived(&self, id: &str, entries: &[Entry]) -> std::io::Result<bool> {
        self.continue_archived_with_state(id, entries, SessionState::default())
    }

    fn continue_archived_with_state(
        &self,
        id: &str,
        entries: &[Entry],
        state: SessionState,
    ) -> std::io::Result<bool> {
        let Some(id) = SessionId::new(id) else {
            return Ok(false);
        };
        self.with_journal(|journal| journal.replace_session(&id, entries.to_vec(), state))?;
        Ok(true)
    }

    fn continue_archived_live(&self, id: &str) -> std::io::Result<bool> {
        let Some(id) = SessionId::new(id) else {
            return Ok(false);
        };
        self.set_current(id);
        Ok(true)
    }

    fn discard(&self) {
        let id = self.current_id();
        let _ = self.with_journal(|journal| {
            journal.replace_session(&id, Vec::new(), SessionState::default())
        });
        self.write_current();
    }

    fn list(&self) -> Vec<SessionMeta> {
        let mut grouped: BTreeMap<SessionId, Vec<session::Record>> = BTreeMap::new();
        for record in self.records() {
            grouped
                .entry(record.session.clone())
                .or_default()
                .push(record);
        }
        let mut out: Vec<SessionMeta> = grouped
            .into_iter()
            .map(|(id, records)| {
                let (title, turns, updated) = session::summarize(&records);
                SessionMeta {
                    id: id.as_str().to_string(),
                    title,
                    updated,
                    turns,
                }
            })
            .collect();
        out.sort_by_key(|meta| std::cmp::Reverse(meta.updated));
        out
    }

    fn load(&self, id: &str) -> Option<Vec<Entry>> {
        let id = SessionId::new(id)?;
        let records = self.records_for(&id);
        if records.is_empty() {
            return None;
        }
        self.set_current(id);
        Some(records.into_iter().map(|record| record.entry).collect())
    }

    fn resume_id(&self) -> Option<String> {
        let id = std::fs::read_to_string(self.resume_path()).ok()?;
        SessionId::new(id.trim()).map(|id| id.as_str().to_string())
    }

    fn set_resume_id(&self, id: &str) {
        if let Some(id) = SessionId::new(id) {
            let _ = crate::atomic_write(&self.resume_path(), id.as_str().as_bytes());
        }
    }

    fn clear_resume_id(&self) {
        let _ = std::fs::remove_file(self.resume_path());
    }
}
