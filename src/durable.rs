use crate::ids::{EntryId, Seq, SessionId};
use crate::session::{self, Entry, Record, SessionState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 1;

const SNAPSHOT_BYTES: u64 = 1024 * 1024;
const STATE: &str = "state.json";
const JOURNAL: &str = "journal.jsonl";
const LEASE: &str = "journal.lock";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StateRecord {
    session: SessionId,
    #[serde(default)]
    state: SessionState,
}

#[derive(Deserialize)]
struct Snapshot {
    version: u32,
    seq: Seq,
    next_id: EntryId,
    entries: Vec<Record>,
    #[serde(default)]
    states: Vec<StateRecord>,
}

#[derive(Serialize)]
struct SnapshotOut<'a> {
    version: u32,
    seq: Seq,
    next_id: EntryId,
    entries: &'a [Record],
    states: Vec<StateRecord>,
}

#[derive(Deserialize)]
struct Commit {
    seq: Seq,
    entries: Vec<Record>,
    #[serde(default)]
    states: Vec<StateRecord>,
}

#[derive(Serialize)]
struct Line<'a> {
    seq: Seq,
    at: i64,
    entries: &'a [Record],
    states: Vec<StateRecord>,
}

pub struct Journal {
    dir: PathBuf,
    lease: File,
    journal: File,
    seq: Seq,
    next_id: EntryId,
    bytes: u64,
    entries: Vec<Record>,
    states: BTreeMap<SessionId, SessionState>,
}

impl Journal {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let lease = lock_dir(dir)?;

        let mut seq = Seq(0);
        let mut next_id = EntryId(0);
        let mut entries = Vec::new();
        let mut states = BTreeMap::new();
        if let Some(snapshot) = read_snapshot(dir)? {
            if snapshot.version != VERSION {
                return Err(invalid(format!(
                    "journal version {} is not {VERSION}",
                    snapshot.version
                )));
            }
            seq = snapshot.seq;
            next_id = snapshot.next_id;
            entries = snapshot.entries;
            for state in snapshot.states {
                states.insert(state.session, state.state);
            }
        }

        let mut journal = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(dir.join(JOURNAL))?;
        let mut raw = Vec::new();
        journal.read_to_end(&mut raw)?;
        let keep = valid_prefix(&raw);
        if keep != raw.len() {
            journal.set_len(keep as u64)?;
            journal.sync_data()?;
            raw.truncate(keep);
        }
        let bytes = keep as u64;
        for line in raw
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let commit: Commit = serde_json::from_slice(line).map_err(invalid)?;
            if commit.seq <= seq {
                continue;
            }
            seq = commit.seq;
            for record in commit.entries {
                next_id = next_id.max(record.id.next());
                entries.push(record);
            }
            for state in commit.states {
                states.insert(state.session, state.state);
            }
        }

        Ok(Self {
            dir: dir.to_path_buf(),
            lease,
            journal,
            seq,
            next_id,
            bytes,
            entries,
            states,
        })
    }

    pub fn seed(dir: &Path, entries: Vec<Entry>) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let _lease = lock_dir(dir)?;
        if dir.join(STATE).exists() || dir.join(JOURNAL).exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "the journal already has state",
            ));
        }
        if entries.is_empty() {
            return Ok(());
        }
        let at = session::now_ms();
        let records: Vec<Record> = entries
            .into_iter()
            .enumerate()
            .map(|(id, entry)| Record {
                id: EntryId(id as u64),
                at,
                session: SessionId::default(),
                entry,
            })
            .collect();
        let snapshot = SnapshotOut {
            version: VERSION,
            seq: Seq(1),
            next_id: EntryId(records.len() as u64),
            entries: &records,
            states: Vec::new(),
        };
        let bytes = serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?;
        crate::atomic_write(&dir.join(STATE), &bytes)
    }

    pub fn load(dir: &Path) -> std::io::Result<Vec<Record>> {
        Ok(read(dir)?.records)
    }

    pub fn load_state(dir: &Path, session: &SessionId) -> std::io::Result<SessionState> {
        Ok(read(dir)?.states.get(session).cloned().unwrap_or_default())
    }

    pub fn entries(&self) -> &[Record] {
        &self.entries
    }

    pub fn seq(&self) -> Seq {
        self.seq
    }

    pub fn journal_bytes(&self) -> u64 {
        self.bytes
    }

    pub fn append(&mut self, entries: Vec<Entry>) -> std::io::Result<Seq> {
        self.commit_session(&SessionId::default(), entries)
    }

    pub fn commit_session(
        &mut self,
        session: &SessionId,
        entries: Vec<Entry>,
    ) -> std::io::Result<Seq> {
        self.commit_session_with_state(session, entries, None)
    }

    pub fn commit_session_with_state(
        &mut self,
        session: &SessionId,
        entries: Vec<Entry>,
        state: Option<SessionState>,
    ) -> std::io::Result<Seq> {
        if entries.is_empty() && state.is_none() {
            return Ok(self.seq);
        }
        let at = session::now_ms();
        let mut records = Vec::with_capacity(entries.len());
        let mut id = self.next_id;
        for entry in entries {
            records.push(Record {
                id,
                at,
                session: session.clone(),
                entry,
            });
            id = id.next();
        }
        let seq = self.seq.next();
        let states = state
            .as_ref()
            .map(|state| {
                vec![StateRecord {
                    session: session.clone(),
                    state: state.clone(),
                }]
            })
            .unwrap_or_default();
        let mut line = serde_json::to_vec(&Line {
            seq,
            at,
            entries: &records,
            states: states.clone(),
        })
        .map_err(std::io::Error::other)?;
        line.push(b'\n');

        let before = self.bytes;
        if let Err(error) = self
            .journal
            .write_all(&line)
            .and_then(|()| self.journal.sync_data())
        {
            let _ = self.journal.set_len(before);
            return Err(error);
        }

        self.bytes = before + line.len() as u64;
        self.seq = seq;
        self.next_id = id;
        self.entries.extend(records);
        if let Some(state) = state {
            self.states.insert(session.clone(), state);
        }
        Ok(seq)
    }

    pub fn replace_session(
        &mut self,
        session: &SessionId,
        entries: Vec<Entry>,
        state: SessionState,
    ) -> std::io::Result<()> {
        let mut kept: Vec<Record> = self
            .entries
            .iter()
            .filter(|record| &record.session != session)
            .cloned()
            .collect();
        let at = session_now_ms();
        let mut id = self.next_id;
        for entry in entries {
            kept.push(Record {
                id,
                at,
                session: session.clone(),
                entry,
            });
            id = id.next();
        }
        let seq = self.seq.next();
        let previous = self.states.insert(session.clone(), state);
        if let Err(error) = self.write_state_at(&kept, seq, id) {
            if let Some(previous) = previous {
                self.states.insert(session.clone(), previous);
            } else {
                self.states.remove(session);
            }
            return Err(error);
        }
        self.entries = kept;
        self.seq = seq;
        self.next_id = id;
        self.truncate_journal()
    }

    pub fn state(&self, session: &SessionId) -> SessionState {
        self.states.get(session).cloned().unwrap_or_default()
    }

    pub fn maybe_snapshot(&mut self) -> std::io::Result<()> {
        self.maybe_snapshot_with(|_| {})
    }

    pub fn maybe_snapshot_with(
        &mut self,
        prune: impl FnOnce(&mut [Record]),
    ) -> std::io::Result<()> {
        if self.bytes < SNAPSHOT_BYTES {
            return Ok(());
        }
        self.snapshot_with(prune)
    }

    pub fn snapshot_with(&mut self, prune: impl FnOnce(&mut [Record])) -> std::io::Result<()> {
        let mut entries = self.entries.clone();
        prune(&mut entries);
        self.write_state(&entries)?;
        self.entries = entries;
        self.truncate_journal()
    }

    pub fn snapshot(&mut self) -> std::io::Result<()> {
        let entries = std::mem::take(&mut self.entries);
        let written = self.write_state(&entries);
        self.entries = entries;
        written?;
        self.truncate_journal()
    }

    fn write_state(&mut self, entries: &[Record]) -> std::io::Result<()> {
        self.write_state_at(entries, self.seq, self.next_id)
    }

    fn write_state_at(
        &mut self,
        entries: &[Record],
        seq: Seq,
        next_id: EntryId,
    ) -> std::io::Result<()> {
        let states: Vec<StateRecord> = self
            .states
            .iter()
            .map(|(session, state)| StateRecord {
                session: session.clone(),
                state: state.clone(),
            })
            .collect();
        let snapshot = SnapshotOut {
            version: VERSION,
            seq,
            next_id,
            entries,
            states,
        };
        let bytes = serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?;
        crate::atomic_write(&self.dir.join(STATE), &bytes)
    }

    fn truncate_journal(&mut self) -> std::io::Result<()> {
        self.journal.set_len(0)?;
        self.journal.sync_data()?;
        self.bytes = 0;
        Ok(())
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.lease.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn lock_dir(dir: &Path) -> std::io::Result<File> {
    let lease = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(LEASE))?;
    let taken = unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if taken != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "the journal is held by another process",
        ));
    }
    Ok(lease)
}

fn valid_prefix(raw: &[u8]) -> usize {
    match raw.iter().rposition(|byte| *byte == b'\n') {
        Some(at) => at + 1,
        None => 0,
    }
}

struct Loaded {
    records: Vec<Record>,
    states: BTreeMap<SessionId, SessionState>,
}

fn read(dir: &Path) -> std::io::Result<Loaded> {
    let mut seq = Seq(0);
    let mut records = Vec::new();
    let mut states = BTreeMap::new();
    if let Some(snapshot) = read_snapshot(dir)? {
        if snapshot.version != VERSION {
            return Err(invalid(format!(
                "journal version {} is not {VERSION}",
                snapshot.version
            )));
        }
        seq = snapshot.seq;
        records = snapshot.entries;
        for state in snapshot.states {
            states.insert(state.session, state.state);
        }
    }
    let raw = match std::fs::read(dir.join(JOURNAL)) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Loaded { records, states });
        }
        Err(error) => return Err(error),
    };
    let keep = valid_prefix(&raw);
    for line in raw[..keep]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let commit: Commit = serde_json::from_slice(line).map_err(invalid)?;
        if commit.seq <= seq {
            continue;
        }
        seq = commit.seq;
        records.extend(commit.entries);
        for state in commit.states {
            states.insert(state.session, state.state);
        }
    }
    Ok(Loaded { records, states })
}

fn read_snapshot(dir: &Path) -> std::io::Result<Option<Snapshot>> {
    let path = dir.join(STATE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let snapshot = serde_json::from_slice(&bytes).map_err(invalid)?;
    Ok(Some(snapshot))
}

fn session_now_ms() -> i64 {
    session::now_ms()
}

fn invalid(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("axe-journal-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    fn usage(input: usize) -> Entry {
        Entry::Message {
            message: crate::Message {
                role: "user".into(),
                content: input.to_string(),
                ..crate::Message::default()
            },
        }
    }

    fn inputs(journal: &Journal) -> Vec<usize> {
        journal
            .entries()
            .iter()
            .filter_map(|record| match &record.entry {
                Entry::Message { message } => message.content.parse().ok(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn entries_survive_a_reopen() {
        let dir = dir("reopen");
        {
            let mut journal = Journal::open(&dir).unwrap();
            journal.append(vec![usage(1)]).unwrap();
            journal.append(vec![usage(2)]).unwrap();
        }
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(inputs(&journal), [1, 2]);
        assert_eq!(journal.seq(), Seq(2));
        assert_eq!(journal.entries()[1].id, EntryId(1));
    }

    #[test]
    fn a_torn_last_line_is_dropped() {
        let dir = dir("torn");
        {
            let mut journal = Journal::open(&dir).unwrap();
            journal.append(vec![usage(1)]).unwrap();
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(JOURNAL))
            .unwrap();
        file.write_all(br#"{"seq":2,"at":0,"entries":[{"id":1"#)
            .unwrap();
        drop(file);

        let mut journal = Journal::open(&dir).unwrap();
        assert_eq!(inputs(&journal), [1]);
        journal.append(vec![usage(2)]).unwrap();
        drop(journal);

        let journal = Journal::open(&dir).unwrap();
        assert_eq!(inputs(&journal), [1, 2]);
        assert_eq!(journal.seq(), Seq(2));
    }

    #[test]
    fn a_snapshot_empties_the_journal_without_losing_entries() {
        let dir = dir("snapshot");
        {
            let mut journal = Journal::open(&dir).unwrap();
            journal.append(vec![usage(1), usage(2)]).unwrap();
            journal.append(vec![usage(3)]).unwrap();
            journal.snapshot().unwrap();
            assert_eq!(journal.journal_bytes(), 0);
        }
        assert_eq!(std::fs::metadata(dir.join(JOURNAL)).unwrap().len(), 0);
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(inputs(&journal), [1, 2, 3]);
    }

    #[test]
    fn commits_before_a_snapshot_are_not_applied_twice() {
        let dir = dir("replay");
        let old = {
            let mut journal = Journal::open(&dir).unwrap();
            journal.append(vec![usage(1)]).unwrap();
            journal.append(vec![usage(2)]).unwrap();
            std::fs::read(dir.join(JOURNAL)).unwrap()
        };
        {
            let mut journal = Journal::open(&dir).unwrap();
            journal.snapshot().unwrap();
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(JOURNAL))
            .unwrap();
        file.write_all(&old).unwrap();
        drop(file);

        let mut journal = Journal::open(&dir).unwrap();
        assert_eq!(inputs(&journal), [1, 2]);
        journal.append(vec![usage(3)]).unwrap();
        assert_eq!(journal.seq(), Seq(3));
        assert_eq!(journal.entries()[2].id, EntryId(2));
    }

    #[test]
    fn one_lease_per_journal() {
        let dir = dir("lease");
        let first = Journal::open(&dir).unwrap();
        let second = Journal::open(&dir);
        assert_eq!(second.err().unwrap().kind(), std::io::ErrorKind::WouldBlock);
        drop(first);
        assert!(Journal::open(&dir).is_ok());
    }

    #[test]
    fn a_snapshot_can_prune_the_entries_it_stores() {
        let dir = dir("prune");
        let mut journal = Journal::open(&dir).unwrap();
        journal.append(vec![usage(1), usage(2)]).unwrap();
        journal
            .snapshot_with(|records| records[0].entry = usage(9))
            .unwrap();
        assert_eq!(inputs(&journal), [9, 2]);
        drop(journal);
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(inputs(&journal), [9, 2]);
    }

    #[test]
    fn a_failed_prune_leaves_the_entries_alone() {
        let dir = dir("prune-fail");
        let mut journal = Journal::open(&dir).unwrap();
        journal.append(vec![usage(1), usage(2)]).unwrap();
        std::fs::create_dir_all(dir.join(STATE)).unwrap();
        assert!(
            journal
                .snapshot_with(|records| records[0].entry = usage(9))
                .is_err()
        );
        assert_eq!(inputs(&journal), [1, 2]);
    }

    #[test]
    fn a_seed_becomes_the_first_snapshot() {
        let dir = dir("seed");
        Journal::seed(&dir, vec![usage(1), usage(2)]).unwrap();
        assert!(Journal::seed(&dir, vec![usage(3)]).is_err());
        let reader = Journal::load(&dir).unwrap();
        assert_eq!(reader.len(), 2);
        let mut journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.seq(), Seq(1));
        journal.append(vec![usage(3)]).unwrap();
        assert_eq!(journal.seq(), Seq(2));
        assert_eq!(journal.entries()[2].id, EntryId(2));
    }

    #[test]
    fn a_reader_sees_committed_entries_without_the_lease() {
        let dir = dir("reader");
        let mut journal = Journal::open(&dir).unwrap();
        journal.append(vec![usage(1)]).unwrap();
        let reader = Journal::load(&dir).unwrap();
        assert_eq!(reader.len(), 1);
        journal.append(vec![usage(2)]).unwrap();
        journal.snapshot().unwrap();
        journal.append(vec![usage(3)]).unwrap();
        let reader = Journal::load(&dir).unwrap();
        assert_eq!(reader.len(), 3);
    }

    #[test]
    #[ignore]
    fn commit_speed() {
        let dir = dir("speed");
        let mut journal = Journal::open(&dir).unwrap();
        let start = std::time::Instant::now();
        for input in 0..1000 {
            journal.append(vec![usage(input)]).unwrap();
        }
        println!("{:?} per commit", start.elapsed() / 1000);
        let start = std::time::Instant::now();
        journal.snapshot().unwrap();
        println!("{:?} to snapshot 1000 entries", start.elapsed());
        let start = std::time::Instant::now();
        drop(journal);
        let journal = Journal::open(&dir).unwrap();
        println!("{:?} to open", start.elapsed());
        assert_eq!(journal.entries().len(), 1000);
    }
}
