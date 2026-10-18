use crate::session::{self, Entry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: u64,
    pub at: i64,
    pub entry: Entry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: u64,
    pub kind: String,
    pub state: TaskState,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub checkpoint: Value,
    pub attempts: u32,
    pub updated: i64,
}

#[derive(Deserialize)]
struct Snapshot {
    version: u32,
    seq: u64,
    next_id: u64,
    entries: Vec<Record>,
    #[serde(default)]
    tasks: Vec<TaskRecord>,
}

#[derive(Serialize)]
struct SnapshotOut<'a> {
    version: u32,
    seq: u64,
    next_id: u64,
    entries: &'a [Record],
    tasks: &'a [TaskRecord],
}

#[derive(Deserialize)]
struct Commit {
    seq: u64,
    entries: Vec<Record>,
    #[serde(default)]
    tasks: Vec<TaskRecord>,
}

#[derive(Serialize)]
struct Line<'a> {
    seq: u64,
    at: i64,
    entries: &'a [Record],
    tasks: &'a [TaskRecord],
}

pub struct Journal {
    dir: PathBuf,
    lease: File,
    journal: File,
    seq: u64,
    next_id: u64,
    next_task: u64,
    bytes: u64,
    entries: Vec<Record>,
    tasks: BTreeMap<u64, TaskRecord>,
}

impl Journal {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
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

        let mut seq = 0;
        let mut next_id = 0;
        let mut next_task = 0;
        let mut entries = Vec::new();
        let mut tasks = BTreeMap::new();
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
            for task in snapshot.tasks {
                next_task = next_task.max(task.id + 1);
                tasks.insert(task.id, task);
            }
        }

        let mut journal = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(dir.join(JOURNAL))?;
        let mut raw = Vec::new();
        journal.read_to_end(&mut raw)?;
        let keep = match raw.iter().rposition(|byte| *byte == b'\n') {
            Some(at) => at + 1,
            None => 0,
        };
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
                next_id = next_id.max(record.id + 1);
                entries.push(record);
            }
            for task in commit.tasks {
                next_task = next_task.max(task.id + 1);
                tasks.insert(task.id, task);
            }
        }

        Ok(Self {
            dir: dir.to_path_buf(),
            lease,
            journal,
            seq,
            next_id,
            next_task,
            bytes,
            entries,
            tasks,
        })
    }

    pub fn seed(dir: &Path, entries: Vec<Entry>) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
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
                id: id as u64,
                at,
                entry,
            })
            .collect();
        let snapshot = SnapshotOut {
            version: VERSION,
            seq: 1,
            next_id: records.len() as u64,
            entries: &records,
            tasks: &[],
        };
        let bytes = serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?;
        crate::atomic_write(&dir.join(STATE), &bytes)
    }

    pub fn load(dir: &Path) -> std::io::Result<Vec<Record>> {
        Ok(read(dir)?.0)
    }

    pub fn load_tasks(dir: &Path) -> std::io::Result<Vec<TaskRecord>> {
        Ok(read(dir)?.1)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn entries(&self) -> &[Record] {
        &self.entries
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn journal_bytes(&self) -> u64 {
        self.bytes
    }

    pub fn append(&mut self, entries: Vec<Entry>) -> std::io::Result<u64> {
        self.commit(entries, Vec::new())
    }

    pub fn commit(&mut self, entries: Vec<Entry>, tasks: Vec<TaskRecord>) -> std::io::Result<u64> {
        if entries.is_empty() && tasks.is_empty() {
            return Ok(self.seq);
        }
        let at = session::now_ms();
        let mut tasks = tasks;
        for task in &mut tasks {
            task.updated = at;
        }
        let mut records = Vec::with_capacity(entries.len());
        let mut id = self.next_id;
        for entry in entries {
            records.push(Record { id, at, entry });
            id += 1;
        }
        let seq = self.seq + 1;
        let mut line = serde_json::to_vec(&Line {
            seq,
            at,
            entries: &records,
            tasks: &tasks,
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
        for task in tasks {
            self.next_task = self.next_task.max(task.id + 1);
            self.tasks.insert(task.id, task);
        }
        Ok(seq)
    }

    pub fn task(&self, id: u64) -> Option<&TaskRecord> {
        self.tasks.get(&id)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &TaskRecord> {
        self.tasks.values()
    }

    pub fn new_task(&mut self) -> u64 {
        let id = self.next_task;
        self.next_task += 1;
        id
    }

    pub fn retain_tasks(&mut self, mut keep: impl FnMut(&TaskRecord) -> bool) -> usize {
        let before = self.tasks.len();
        self.tasks.retain(|_, task| keep(task));
        before - self.tasks.len()
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
        prune(&mut self.entries);
        self.snapshot()
    }

    pub fn snapshot(&mut self) -> std::io::Result<()> {
        let tasks: Vec<&TaskRecord> = self.tasks.values().collect();
        let snapshot = SnapshotOut {
            version: VERSION,
            seq: self.seq,
            next_id: self.next_id,
            entries: &self.entries,
            tasks: &tasks.into_iter().cloned().collect::<Vec<TaskRecord>>(),
        };
        let bytes = serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?;
        crate::atomic_write(&self.dir.join(STATE), &bytes)?;
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

fn read(dir: &Path) -> std::io::Result<(Vec<Record>, Vec<TaskRecord>)> {
    let mut seq = 0;
    let mut records = Vec::new();
    let mut tasks = BTreeMap::new();
    if let Some(snapshot) = read_snapshot(dir)? {
        if snapshot.version != VERSION {
            return Err(invalid(format!(
                "journal version {} is not {VERSION}",
                snapshot.version
            )));
        }
        seq = snapshot.seq;
        records = snapshot.entries;
        for task in snapshot.tasks {
            tasks.insert(task.id, task);
        }
    }
    let raw = match std::fs::read(dir.join(JOURNAL)) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((records, tasks.into_values().collect()));
        }
        Err(error) => return Err(error),
    };
    let keep = match raw.iter().rposition(|byte| *byte == b'\n') {
        Some(at) => at + 1,
        None => 0,
    };
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
        for task in commit.tasks {
            tasks.insert(task.id, task);
        }
    }
    Ok((records, tasks.into_values().collect()))
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
        Entry::Usage {
            input,
            output: 0,
            cached_input: 0,
            context_input: 0,
            context_output: 0,
        }
    }

    fn inputs(journal: &Journal) -> Vec<usize> {
        journal
            .entries()
            .iter()
            .filter_map(|record| match &record.entry {
                Entry::Usage { input, .. } => Some(*input),
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
        assert_eq!(journal.seq(), 2);
        assert_eq!(journal.entries()[1].id, 1);
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
        assert_eq!(journal.seq(), 2);
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
        assert_eq!(journal.seq(), 3);
        assert_eq!(journal.entries()[2].id, 2);
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

    fn task(id: u64) -> TaskRecord {
        TaskRecord {
            id,
            kind: "turn".into(),
            state: TaskState::Running,
            phase: "request".into(),
            checkpoint: Value::Null,
            attempts: 0,
            updated: 0,
        }
    }

    #[test]
    fn a_task_is_committed_with_its_entries() {
        let dir = dir("tasks");
        let mut journal = Journal::open(&dir).unwrap();
        let id = journal.new_task();
        journal.commit(vec![usage(1)], vec![task(id)]).unwrap();
        assert_eq!(journal.task(id).unwrap().state, TaskState::Running);
        assert_eq!(journal.tasks().count(), 1);
        drop(journal);

        let mut journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.task(id).unwrap().phase, "request");
        assert_eq!(inputs(&journal), [1]);
        assert_eq!(journal.new_task(), id + 1);
    }

    #[test]
    fn retained_tasks_are_dropped_by_a_snapshot() {
        let dir = dir("retain");
        let kept;
        {
            let mut journal = Journal::open(&dir).unwrap();
            let first = journal.new_task();
            kept = journal.new_task();
            journal
                .commit(vec![], vec![task(first), task(kept)])
                .unwrap();
            assert_eq!(journal.retain_tasks(|task| task.id == kept), 1);
            journal.snapshot().unwrap();
        }
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(
            journal.tasks().map(|task| task.id).collect::<Vec<_>>(),
            [kept]
        );
    }

    #[test]
    fn a_task_update_survives_a_snapshot() {
        let dir = dir("tasks-snapshot");
        let id;
        {
            let mut journal = Journal::open(&dir).unwrap();
            id = journal.new_task();
            journal.commit(Vec::new(), vec![task(id)]).unwrap();
        }
        {
            let mut journal = Journal::open(&dir).unwrap();
            let mut updated = journal.task(id).unwrap().clone();
            updated.state = TaskState::Done;
            updated.attempts = 2;
            journal.commit(Vec::new(), vec![updated]).unwrap();
            journal.snapshot().unwrap();
        }
        let journal = Journal::open(&dir).unwrap();
        let task = journal.task(id).unwrap();
        assert_eq!(task.state, TaskState::Done);
        assert_eq!(task.attempts, 2);
        drop(journal);
        assert_eq!(Journal::load_tasks(&dir).unwrap().len(), 1);
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
    fn a_seed_becomes_the_first_snapshot() {
        let dir = dir("seed");
        Journal::seed(&dir, vec![usage(1), usage(2)]).unwrap();
        assert!(Journal::seed(&dir, vec![usage(3)]).is_err());
        let reader = Journal::load(&dir).unwrap();
        assert_eq!(reader.len(), 2);
        let mut journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.seq(), 1);
        journal.append(vec![usage(3)]).unwrap();
        assert_eq!(journal.seq(), 2);
        assert_eq!(journal.entries()[2].id, 2);
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
