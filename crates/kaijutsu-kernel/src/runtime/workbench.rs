//! Publish successful shell text writes as command-owned diff blocks.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use kaijutsu_cas::ContentHash;
use kaijutsu_diff::{DiffModel, DiffOptions, FileSpec, Truncation};
use kaijutsu_types::{BlockId, BlockKind, ContentType, Role, Status};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::block_store::SharedBlockStore;

pub(crate) const MAX_TEXT_BYTES: usize = 256 * 1024;

const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const QUEUE_CAPACITY: usize = 16;
const MAX_PENDING_SEGMENTS: usize = 32;
const RETAINED_BYTES: usize = 8 * 1024 * 1024;
const MAX_HOT_PATHS: usize = 32;

#[derive(Clone)]
pub(crate) struct WriteRecorder {
    sender: mpsc::Sender<Message>,
}

enum Message {
    Change(Change),
    Finish(oneshot::Sender<Result<(), String>>),
}

struct Change {
    path: String,
    segment: Option<Segment>,
}

#[derive(Default)]
struct Pending {
    paths: BTreeMap<String, Vec<Option<Segment>>>,
    segments: usize,
}

impl Pending {
    fn add(&mut self, change: Change) {
        let sequence = self.paths.entry(change.path).or_default();
        match (sequence.last_mut(), change.segment) {
            (Some(Some(active)), Some(next)) if next.before.as_ref() == Some(&active.after) => {
                active.after = next.after;
            }
            (Some(None), None) => {}
            (_, segment) => {
                sequence.push(segment);
                self.segments += 1;
            }
        }
    }
}

impl WriteRecorder {
    #[cfg(test)]
    pub(crate) fn start(store: SharedBlockStore, command: BlockId) -> Self {
        Self::start_in(store, command, std::env::temp_dir())
    }

    pub(crate) fn start_in(store: SharedBlockStore, command: BlockId, spill_root: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        tokio::spawn(run(store, command, receiver, spill_root));
        Self { sender }
    }

    pub(crate) async fn observe(&self, path: String, before: Option<String>, after: String) {
        if before.as_ref() == Some(&after) {
            return;
        }
        if after.len() > MAX_TEXT_BYTES || before.as_ref().is_some_and(|text| text.len() > MAX_TEXT_BYTES) {
            self.unavailable(path).await;
            return;
        }
        self.send(Change { path, segment: Some(Segment { before, after }) }).await;
    }

    pub(crate) async fn unavailable(&self, path: String) {
        self.send(Change { path, segment: None }).await;
    }

    async fn send(&self, change: Change) {
        if self.sender.send(Message::Change(change)).await.is_err() {
            tracing::warn!("shell write diff worker stopped before recording a successful write");
        }
    }

    pub(crate) async fn finish(&self) -> Result<(), String> {
        let (ack, done) = oneshot::channel();
        let stopped = || "shell write diff worker stopped before the final flush".to_string();
        self.sender.send(Message::Finish(ack)).await.map_err(|_| stopped())?;
        done.await.map_err(|_| stopped())?
    }
}

#[derive(Default, serde::Deserialize, serde::Serialize)]
struct PathDiff {
    sealed: String,
    active: Option<Segment>,
    block: Option<(BlockId, ContentHash)>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Segment {
    before: Option<String>,
    after: String,
}

fn render_segment(path: &str, segment: &Segment, model: &mut DiffModel) {
    if segment.before.as_ref() == Some(&segment.after) {
        return;
    }
    let spec = match &segment.before {
        Some(before) => FileSpec::modified(path, before, &segment.after),
        None => FileSpec::added(path, &segment.after),
    };
    match kaijutsu_diff::diff_file(&spec, &diff_options()) {
        Ok(file) => model.files.push(file),
        Err(error) => {
            model.truncated.get_or_insert_with(Truncation::default).omitted_files += 1;
            tracing::warn!(%path, %error, "shell write could not be represented as a diff");
        }
    }
}

fn diff_options() -> DiffOptions {
    DiffOptions { max_refine_region_bytes: 0, ..DiffOptions::default() }
}

fn enforce_output_limit(model: &mut DiffModel) {
    let budget = kaijutsu_diff::limits::MAX_GENERATED_BYTES;
    if kaijutsu_diff::format(model).len() > budget {
        *model = kaijutsu_diff::truncate_to_bytes(model, budget);
    }
}

impl PathDiff {
    fn retained_bytes(&self) -> usize {
        self.sealed.capacity() + self.active.as_ref().map_or(0, |segment| {
            segment.before.as_ref().map_or(0, String::capacity) + segment.after.capacity()
        })
    }

    fn apply(&mut self, path: &str, before: Option<String>, after: String) -> anyhow::Result<()> {
        if let Some(active) = &mut self.active
            && before.as_ref() == Some(&active.after)
        {
            active.after = after;
            return Ok(());
        }
        self.seal(path, false)?;
        self.active = Some(Segment { before, after });
        Ok(())
    }

    fn seal(&mut self, path: &str, unavailable: bool) -> anyhow::Result<()> {
        if self.active.is_none() && !unavailable {
            return Ok(());
        }
        let mut sealed = kaijutsu_diff::parse_with(&self.sealed, &diff_options())?;
        if let Some(active) = self.active.take() {
            // A different pre-image starts a new section, so a change in the
            // gap never becomes part of this command's attributed writes.
            render_segment(path, &active, &mut sealed);
        }
        if unavailable {
            sealed.truncated.get_or_insert_with(Truncation::default).omitted_files += 1;
        }
        enforce_output_limit(&mut sealed);
        self.sealed = kaijutsu_diff::format(&sealed);
        Ok(())
    }

    fn publish(&mut self, store: &SharedBlockStore, command: BlockId, path: &str) -> anyhow::Result<()> {
        let context = command.context_id;
        let mut model = kaijutsu_diff::parse_with(&self.sealed, &diff_options())?;
        if let Some(active) = &self.active {
            render_segment(path, active, &mut model);
        }
        if model.files.is_empty() && model.truncated.is_none() && self.block.is_none() {
            return Ok(());
        }
        enforce_output_limit(&mut model);
        let content = kaijutsu_diff::format(&model);
        let hash = ContentHash::from_data(content.as_bytes());
        if let Some((id, previous)) = &mut self.block {
            if *previous == hash {
                return Ok(());
            }
            let current = store.get_block_snapshot(context, id)?
                .with_context(|| format!("diff block {id} disappeared before update"))?;
            if ContentHash::from_data(current.content.as_bytes()) != *previous {
                anyhow::bail!("diff block {id} was edited independently");
            }
            store.replace_text_if_unchanged_as(context, id, &current.content, &content, None)?;
            *previous = hash;
        } else {
            let after = store.last_block_id(context);
            let id = store.insert_block(
                context, Some(&command), after.as_ref(), Role::Asset, BlockKind::Trace,
                &content, Status::Done, ContentType::Diff,
            )?;
            self.block = Some((id, hash));
        }
        Ok(())
    }
}

// Recent paths stay in memory. Cold paths keep their exact baseline, latest
// text, sealed sections, and block identity until the command finishes.
struct RetainedPaths {
    hot: VecDeque<(String, PathDiff)>,
    used: usize,
    budget: usize,
    spill_root: PathBuf,
    directory: Option<tempfile::TempDir>,
}

impl RetainedPaths {
    fn new(budget: usize, spill_root: PathBuf) -> Self {
        Self { hot: VecDeque::new(), used: 0, budget, spill_root, directory: None }
    }

    fn spill_name(path: &str) -> ContentHash {
        ContentHash::from_data(path.as_bytes())
    }

    fn take(&mut self, path: &str) -> anyhow::Result<Option<PathDiff>> {
        if let Some(index) = self.hot.iter().position(|(key, _)| key == path) {
            let (key, diff) = self.hot.remove(index).expect("matching hot path exists");
            self.used -= key.capacity() + diff.retained_bytes();
            return Ok(Some(diff));
        }
        let Some(directory) = &self.directory else { return Ok(None) };
        let name = Self::spill_name(path);
        let file = match File::open(directory.path().join(name.as_str())) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let (key, diff): (String, PathDiff) = serde_json::from_reader(BufReader::new(file))?;
        anyhow::ensure!(key == path, "two shell write paths have the same spill name");
        Ok(Some(diff))
    }

    fn put(&mut self, path: String, diff: PathDiff) -> anyhow::Result<()> {
        let bytes = path.capacity() + diff.retained_bytes();
        if bytes > self.budget || self.budget == 0 {
            return self.spill(&path, &diff);
        }
        while self.used + bytes > self.budget || self.hot.len() == MAX_HOT_PATHS {
            let (key, cold) = self.hot.pop_front().expect("a full cache has a cold path");
            self.used -= key.capacity() + cold.retained_bytes();
            self.spill(&key, &cold)?;
        }
        self.hot.push_back((path, diff));
        self.used += bytes;
        Ok(())
    }

    fn spill(&mut self, path: &str, diff: &PathDiff) -> anyhow::Result<()> {
        let directory = match &mut self.directory {
            Some(directory) => directory,
            directory @ None => {
                let mut builder = tempfile::Builder::new();
                builder.prefix("kaijutsu-diff-");
                #[cfg(unix)]
                builder.permissions(std::fs::Permissions::from_mode(0o700));
                std::fs::create_dir_all(&self.spill_root)?;
                directory.insert(builder.tempdir_in(&self.spill_root)?)
            }
        };
        let name = Self::spill_name(path);
        let mut file = tempfile::NamedTempFile::new_in(directory.path())?;
        let mut output = BufWriter::new(file.as_file_mut());
        serde_json::to_writer(&mut output, &(path, diff))?;
        output.flush()?;
        drop(output);
        file.persist(directory.path().join(name.as_str()))?;
        Ok(())
    }
}

struct WorkerState {
    paths: RetainedPaths,
    failures: BTreeMap<String, String>,
    stopped: Option<String>,
}

impl WorkerState {
    fn new(budget: usize, spill_root: PathBuf) -> Self {
        Self { paths: RetainedPaths::new(budget, spill_root), failures: BTreeMap::new(), stopped: None }
    }

    fn result(&self) -> Result<(), String> {
        let details: Vec<_> = self.stopped.iter().cloned().chain(self.failures.iter().map(|(path, detail)| {
            format!("{path}: {detail}")
        })).collect();
        if details.is_empty() { Ok(()) } else { Err(details.join("\n")) }
    }

    async fn finish(self) -> Result<(), String> {
        // Removing a large spill directory is also blocking filesystem work.
        tokio::task::spawn_blocking(move || self.result()).await
            .map_err(|error| format!("write diff cleanup failed: {error}"))?
    }

    fn publish(
        &mut self, store: &SharedBlockStore, command: BlockId,
        path: String, segments: Vec<Option<Segment>>,
    ) -> anyhow::Result<()> {
        let label = path.trim_start_matches('/');
        let mut state = self.paths.take(&path).context("could not read retained write snapshots")?;
        for segment in segments {
            if let Some(Segment { before, after }) = segment {
                state.get_or_insert_with(PathDiff::default).apply(label, before, after)?;
            } else if let Some(state) = &mut state {
                state.seal(label, true)?;
            }
        }
        let Some(mut state) = state else { return Ok(()) };
        match state.publish(store, command, label) {
            Ok(()) => { self.failures.remove(&path); }
            Err(error) => {
                tracing::warn!(%command, %path, %error, "shell write diff could not be published");
                anyhow::ensure!(self.failures.contains_key(&path) || self.failures.len() < MAX_HOT_PATHS,
                    "too many write diff publications failed");
                self.failures.insert(path.clone(), error.to_string());
            }
        }
        self.paths.put(path, state).context("could not retain write snapshots")
    }
}

async fn run(
    store: SharedBlockStore, command: BlockId, mut receiver: mpsc::Receiver<Message>,
    spill_root: PathBuf,
) {
    let mut state = WorkerState::new(RETAINED_BYTES, spill_root);
    let mut pending = Pending::default();
    let mut deadline = None;
    loop {
        let message = match deadline {
            Some(at) => tokio::select! {
                biased;
                _ = tokio::time::sleep_until(at) => {
                    state = flush(&store, command, state, std::mem::take(&mut pending)).await;
                    deadline = None;
                    continue;
                }
                message = receiver.recv() => message,
            },
            None => receiver.recv().await,
        };
        match message {
            Some(Message::Change(_)) if state.stopped.is_some() => {}
            Some(Message::Change(change)) => {
                deadline.get_or_insert_with(|| Instant::now() + FLUSH_INTERVAL);
                pending.add(change);
                // Each segment carries at most two MAX_TEXT_BYTES snapshots.
                // Flush early under pressure from distinct paths or segments.
                if pending.segments == MAX_PENDING_SEGMENTS {
                    state = flush(&store, command, state, std::mem::take(&mut pending)).await;
                    deadline = None;
                }
            }
            Some(Message::Finish(ack)) => {
                state = flush(&store, command, state, pending).await;
                let _ = ack.send(state.finish().await);
                return;
            }
            None => {
                state = flush(&store, command, state, pending).await;
                if let Err(error) = state.finish().await {
                    tracing::warn!(%command, %error, "shell write diff worker could not finish");
                }
                return;
            }
        }
    }
}

async fn flush(
    store: &SharedBlockStore,
    command: BlockId,
    mut state: WorkerState,
    mut pending: Pending,
) -> WorkerState {
    if state.stopped.is_some() || (pending.segments == 0 && state.failures.is_empty()) {
        return state;
    }
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        for path in state.failures.keys() {
            pending.paths.entry(path.clone()).or_default();
        }
        for (path, segments) in pending.paths {
            if let Err(error) = state.publish(&store, command, path.clone(), segments) {
                let error = format!("{path}: {error:#}");
                tracing::warn!(%command, %error, "shell write diff worker stopped");
                state.stopped = Some(error);
                break;
            }
        }
        state
    }).await.expect("shell write diff worker panicked")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::block_store::BlockStore;
    use crate::flows::{BlockFlow, FlowBus};
    use kaijutsu_types::{ContextId, DocKind, PrincipalId};

    #[test]
    fn a_revert_clears_the_existing_artifact_and_later_writes_reuse_it() {
        let context = ContextId::new();
        let store = Arc::new(BlockStore::new(PrincipalId::new()));
        store.create_document(context, DocKind::Conversation, None).unwrap();
        let command = store.insert_block(context, None, None, Role::User, BlockKind::ToolCall,
            "write", Status::Running, ContentType::Plain).unwrap();
        let dialogue = store.insert_block(context, None, Some(&command), Role::User, BlockKind::Text,
            "later dialogue", Status::Done, ContentType::Plain).unwrap();
        let mut path = PathDiff::default();
        path.apply("file", Some("old\n".into()), "new\n".into()).unwrap();
        path.publish(&store, command, "file").unwrap();
        let id = path.block.as_ref().unwrap().0;
        let blocks = store.block_snapshots(context).unwrap();
        assert_eq!(blocks.iter().map(|block| block.id).collect::<Vec<_>>(), [command, dialogue, id]);
        assert_eq!(blocks[2].parent_id, Some(command));

        path.apply("file", Some("new\n".into()), "old\n".into()).unwrap();
        path.publish(&store, command, "file").unwrap();
        let reverted = store.get_block_snapshot(context, &id).unwrap().unwrap();
        assert!(kaijutsu_diff::parse(&reverted.content).unwrap().files.is_empty());

        path.apply("file", Some("old\n".into()), "final\n".into()).unwrap();
        path.publish(&store, command, "file").unwrap();
        assert_eq!(store.block_snapshots(context).unwrap().len(), 3);
        assert!(store.get_block_snapshot(context, &id).unwrap().unwrap().content.contains("-old\n+final\n"));
    }

    #[tokio::test]
    async fn a_tail_diff_does_not_mask_the_failed_command_status() {
        let context = ContextId::new();
        let store = Arc::new(BlockStore::new(PrincipalId::new()));
        store.create_document(context, DocKind::Conversation, None).unwrap();
        let command = store.insert_block(context, None, None, Role::User, BlockKind::ToolCall,
            "write", Status::Running, ContentType::Plain).unwrap();
        let recorder = WriteRecorder::start(store.clone(), command);
        recorder.observe("/file".into(), None, "new\n".into()).await;
        recorder.finish().await.unwrap();
        store.set_status(context, &command, Status::Error).unwrap();
        assert_eq!(store.live_status(context), Status::Error);
    }

    #[tokio::test]
    async fn an_uncaptured_write_marks_the_tracked_diff_incomplete() {
        let context = ContextId::new();
        let store = Arc::new(BlockStore::new(PrincipalId::new()));
        store.create_document(context, DocKind::Conversation, None).unwrap();
        let command = store.insert_block(context, None, None, Role::User, BlockKind::ToolCall,
            "write", Status::Running, ContentType::Plain).unwrap();
        let recorder = WriteRecorder::start(store.clone(), command);
        recorder.observe("/file".into(), Some("original\n".into()), "first\n".into()).await;
        recorder.unavailable("/file".into()).await;
        recorder.unavailable("/uncaptured".into()).await;
        recorder.observe("/file".into(), Some("first\n".into()), "final\n".into()).await;
        recorder.finish().await.unwrap();

        let blocks = store.block_snapshots(context).unwrap();
        assert_eq!(blocks.len(), 2);
        let patch = kaijutsu_diff::parse(&blocks[1].content).unwrap();
        assert_eq!(patch.files.len(), 2);
        assert_eq!(patch.truncated.unwrap().omitted_files, 1);
    }

    #[tokio::test]
    async fn a_spilled_file_keeps_its_baseline_and_artifact_on_revisit() {
        let context = ContextId::new();
        let store = Arc::new(BlockStore::new(PrincipalId::new()));
        store.create_document(context, DocKind::Conversation, None).unwrap();
        let command = store.insert_block(context, None, None, Role::User, BlockKind::ToolCall,
            "write", Status::Running, ContentType::Plain).unwrap();

        let mut worker = WorkerState::new(24, std::env::temp_dir());
        let mut first = Pending::default();
        for (path, before, after) in [("/file", "old\n", "first\n"), ("/other", "before\n", "after\n")] {
            first.add(Change { path: path.into(), segment: Some(Segment {
                before: Some(before.into()), after: after.into(),
            }) });
        }
        worker = flush(&store, command, worker, first).await;
        assert!(worker.result().is_ok());
        assert_eq!(worker.paths.hot.len(), 1);
        assert_eq!(worker.paths.hot.front().unwrap().0, "/other");
        assert!(worker.paths.used <= worker.paths.budget);
        let first_patch = store.block_snapshots(context).unwrap().into_iter()
            .find(|block| block.content.contains("--- a/file\n")).unwrap();

        let mut next = Pending::default();
        next.add(Change { path: "/file".into(), segment: Some(Segment {
            before: Some("first\n".into()), after: "last\n".into(),
        }) });
        worker = flush(&store, command, worker, next).await;
        assert!(worker.result().is_ok());
        let blocks = store.block_snapshots(context).unwrap();
        assert_eq!(blocks.len(), 3);
        let current = blocks.iter().find(|block| block.id == first_patch.id).unwrap();
        assert!(current.content.contains("-old\n+last\n"));
        assert_eq!(kaijutsu_diff::parse(&current.content).unwrap().files.len(), 1);

        for (before, after) in [("external", "third"), ("third", "fourth")] {
            let mut pending = Pending::default();
            pending.add(Change { path: "/file".into(), segment: Some(Segment {
                before: Some(format!("{before}\n")), after: format!("{after}\n"),
            }) });
            worker = flush(&store, command, worker, pending).await;
        }
        let directory = worker.paths.directory.as_ref().unwrap().path().to_owned();
        worker.finish().await.unwrap();
        assert!(!directory.exists());
        let current = store.get_block_snapshot(context, &first_patch.id).unwrap().unwrap();
        assert_eq!(kaijutsu_diff::parse(&current.content).unwrap().files.len(), 2);
        assert!(current.content.contains("-old\n+last\n"));
        assert!(current.content.contains("-external\n+fourth\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn publishes_on_first_deadline_and_drains_into_the_same_artifact() {
        let context = ContextId::new();
        let flows = Arc::new(FlowBus::new(16));
        let store = Arc::new(BlockStore::with_flows(PrincipalId::new(), flows.clone()));
        store.create_document(context, DocKind::Conversation, None).unwrap();
        let command = store.insert_block(context, None, None, Role::User, BlockKind::ToolCall,
            "write", Status::Running, ContentType::Plain).unwrap();
        let mut inserted = flows.subscribe("block.>");
        let recorder = WriteRecorder::start(store.clone(), command);
        recorder.observe("/file".into(), Some("original\n".into()), "first\n".into()).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(75)).await;
        let mut previous = "first\n".to_string();
        for step in 0..50 {
            let after = format!("step {step}\n");
            recorder.observe("/file".into(), Some(previous), after.clone()).await;
            previous = after;
        }
        recorder.observe("/file".into(), Some(previous), "second\n".into()).await;
        tokio::task::yield_now().await;
        assert_eq!(store.block_snapshots(context).unwrap().len(), 1);
        tokio::time::advance(Duration::from_millis(26)).await;
        // Keep virtual time at the first deadline while the blocking worker
        // commits and publishes, so a sliding deadline cannot pass this test.
        let first = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(message) = inserted.try_recv()
                    && let BlockFlow::Inserted { block, .. } = message.payload
                {
                    return block;
                }
                assert!(std::time::Instant::now() < deadline, "diff missed its first deadline");
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        while !first.is_finished() {
            tokio::task::yield_now().await;
        }
        let first = first.await.unwrap();
        assert_eq!(first.parent_id, Some(command));
        assert_eq!(first.content_type, ContentType::Diff);
        assert!(!crate::llm::hydrate::is_history_eligible(&first));
        assert!(first.content.contains("+second\n"));

        recorder.observe("/file".into(), Some("other writer\n".into()), "final\n".into()).await;
        recorder.finish().await.unwrap();
        let blocks = store.block_snapshots(context).unwrap();
        assert_eq!(blocks.len(), 2);
        let artifact = &blocks[1];
        assert_eq!(artifact.id, first.id);
        let patch = kaijutsu_diff::parse(&artifact.content).unwrap();
        assert_eq!(patch.files.len(), 2);
        assert!(artifact.content.contains("+second\n"));
        assert!(artifact.content.contains("-other writer\n+final\n"));
        assert!(!artifact.content.contains("+other writer\n"));
    }
}
