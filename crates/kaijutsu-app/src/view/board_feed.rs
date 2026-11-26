//! Quiet reads for the active Boards screen.

use std::{collections::HashMap, time::Duration};

use bevy::prelude::*;
use bevy::tasks::{IoTaskPool, Task, block_on, futures_lite::future};
use kaijutsu_client::{ActorHandle, ConnectionStatus};
use kaijutsu_types::ContextId;
use serde::Deserialize;
use serde_json::Value;

use crate::connection::{RpcActor, RpcConnectionState};

const TOPIC_LIMIT: &str = "24";
const POST_LIMIT: &str = "8";
const REFRESH: Duration = Duration::from_secs(4);
const READ_WARNING: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BoardTopic {
    pub id: String,
    pub title: String,
    pub path: String,
    pub post_count: usize,
    pub status: String,
    pub age: String,
}

impl BoardTopic {
    pub fn board(&self) -> &str {
        self.path.rsplit_once('/').map_or(&self.path, |(board, _)| board)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BoardPost {
    pub number: usize,
    pub author: String,
    pub body: String,
    pub created_at: u64,
}

#[derive(PartialEq, Eq)]
struct FeedSource {
    generation: u64,
    epoch: u64,
    context: ContextId,
}

type TopicResult = Result<Vec<BoardTopic>, String>;
type PostResult = Result<TopicDetail, String>;

#[derive(Default, Resource)]
pub(super) struct BoardFeed {
    topics: Vec<BoardTopic>,
    focus: Option<String>,
    focus_revision: u64,
    posts: Option<Vec<BoardPost>>,
    resolved_id: Option<String>,
    source: Option<FeedSource>,
    received_topics: bool,
    topic_error: Option<String>,
    post_error: Option<String>,
    next_topics: Duration,
    next_posts: Duration,
    topic_task: Option<(Duration, Task<TopicResult>)>,
    post_task: Option<(u64, Duration, Task<PostResult>)>,
}

impl BoardFeed {
    pub fn topics(&self) -> &[BoardTopic] {
        &self.topics
    }

    pub fn selected(&self) -> Option<&BoardTopic> {
        let focus = self.focus.as_deref()?;
        self.topics.iter().find(|topic| topic.id == focus)
    }

    pub fn focus(&mut self, id: &str) {
        if self.focus.as_deref() == Some(id) || !self.topics.iter().any(|topic| topic.id == id) {
            return;
        }
        self.clear_focus();
        self.focus = Some(id.to_string());
    }

    pub fn clear_focus(&mut self) {
        self.focus = None;
        self.focus_revision += 1;
        self.posts = None;
        self.resolved_id = None;
        self.post_error = None;
        self.next_posts = Duration::ZERO;
    }

    pub fn posts(&self) -> &[BoardPost] {
        self.posts.as_deref().unwrap_or(&[])
    }

    pub fn error(&self) -> Option<&str> {
        self.post_error.as_deref().or(self.topic_error.as_deref())
    }

    pub fn loading(&self) -> bool {
        self.error().is_none() && (!self.received_topics || (self.focus.is_some() && self.posts.is_none()))
    }

    fn replace_topics(&mut self, topics: Vec<BoardTopic>) {
        if let Some(selected) = self.selected()
            && !topics.iter().any(|topic| topic.id == selected.id && topic.path == selected.path)
        {
            self.clear_focus();
        }
        self.topics = topics;
    }

    fn accept_topics(&mut self, result: TopicResult) -> bool {
        match result {
            Ok(topics) => {
                let cleared_error = self.topic_error.take().is_some();
                let changed = !self.received_topics || cleared_error || self.topics != topics;
                self.received_topics = true;
                if changed {
                    self.replace_topics(topics);
                }
                changed
            }
            Err(error) => replace_error(&mut self.topic_error, error),
        }
    }

    fn accept_posts(&mut self, revision: u64, result: PostResult) -> bool {
        if revision != self.focus_revision {
            return false;
        }
        match result {
            Ok(detail) => {
                let changed = self.post_error.take().is_some() || self.posts.as_ref() != Some(&detail.posts);
                self.resolved_id = Some(detail.document_id);
                self.posts = Some(detail.posts);
                changed
            }
            Err(error) => {
                let cleared_posts = self.posts.take().is_some();
                replace_error(&mut self.post_error, error) || cleared_posts
            }
        }
    }
}

fn replace_error(slot: &mut Option<String>, error: String) -> bool {
    if slot.as_ref() == Some(&error) {
        return false;
    }
    *slot = Some(error);
    true
}

pub(super) fn reset_board_feed(mut feed: ResMut<BoardFeed>) {
    *feed = BoardFeed::default();
}

pub(super) fn poll_board_feed(
    actor: Option<Res<RpcActor>>,
    connection: Res<RpcConnectionState>,
    clock: Res<Time<Real>>,
    mut resource: ResMut<BoardFeed>,
) {
    let feed = resource.bypass_change_detection();
    let live = actor.as_deref().filter(|actor| {
        connection.connected && matches!(actor.handle.current_status(), ConnectionStatus::Connected { .. })
    }).zip(connection.context_id);
    let Some((actor, context)) = live else {
        if feed.source.is_some() || feed.topic_error.is_none() {
            *feed = BoardFeed::default();
            feed.topic_error = Some("Boards are unavailable while disconnected.".into());
            resource.set_changed();
        }
        return;
    };

    let source = FeedSource {
        generation: actor.generation,
        epoch: actor.handle.connection_epoch(),
        context,
    };
    let mut changed = feed.source.as_ref() != Some(&source);
    if changed {
        *feed = BoardFeed::default();
        feed.source = Some(source);
    }

    let now = clock.elapsed();
    // Report a slow read promptly and keep its slot until the actor replies.
    // Submitting another read could leave both running in the kernel.
    if let Some((_, mut task)) = feed.topic_task.take_if(|(_, task)| task.is_finished()) {
        let result = block_on(future::poll_once(&mut task)).expect("finished board topic read");
        changed |= feed.accept_topics(result);
        feed.next_topics = now + REFRESH;
    } else if feed.topic_task.as_ref().is_some_and(|(warn_at, _)| now >= *warn_at) {
        changed |= feed.accept_topics(Err("The board activity read is taking longer than expected.".into()));
    }
    if let Some((revision, _, mut task)) = feed.post_task.take_if(|(_, _, task)| task.is_finished()) {
        let result = block_on(future::poll_once(&mut task)).expect("finished board post read");
        changed |= feed.accept_posts(revision, result);
        if revision == feed.focus_revision {
            feed.next_posts = now + REFRESH;
        }
    } else if let Some((revision, warn_at, _)) = &feed.post_task && now >= *warn_at {
        changed |= feed.accept_posts(*revision, Err("The board post read is taking longer than expected.".into()));
    }

    if feed.topic_task.is_none() && now >= feed.next_topics {
        let handle = actor.handle.clone();
        let task = IoTaskPool::get().spawn(async move { read_topics(&handle, context).await });
        feed.topic_task = Some((now + READ_WARNING, task));
    }
    if feed.post_task.is_none() && now >= feed.next_posts && let Some(topic) = feed.selected().cloned() {
        let handle = actor.handle.clone();
        let full_id = feed.resolved_id.clone();
        let task = IoTaskPool::get().spawn(async move {
            read_posts(&handle, context, &topic, full_id.as_deref()).await
        });
        feed.post_task = Some((feed.focus_revision, now + READ_WARNING, task));
    }
    if changed {
        resource.set_changed();
    }
}

async fn quiet_board(
    handle: &ActorHandle,
    context: ContextId,
    args: &[&str],
) -> Result<(Value, String), String> {
    let argv = std::iter::once("board").chain(args.iter().copied()).map(str::to_string).collect();
    let result = handle.execute_kj_quiet(context, argv).await.map_err(|err| err.to_string())?;
    if result.exit_code != 0 {
        return Err(if result.stderr.is_empty() { result.stdout } else { result.stderr });
    }
    let data = result.data.ok_or_else(|| "The board read returned no data.".to_string())?;
    Ok((data, result.stdout))
}

async fn read_topics(handle: &ActorHandle, context: ContextId) -> TopicResult {
    let (data, labels) = quiet_board(handle, context, &["recent", "--limit", TOPIC_LIMIT]).await?;
    decode_topics(data, &labels)
}

fn decode_topics(data: Value, labels: &str) -> TopicResult {
    let error = || "Could not read recent board topics.".to_string();
    let ids: Vec<String> = serde_json::from_value(data).map_err(|_| error())?;
    let mut topics = Vec::with_capacity(ids.len());
    let mut rest = labels.trim_end_matches(['\r', '\n']);
    // The list returns topic ID prefixes in data and display labels on stdout.
    // Find the next row by its ID so line breaks in a title stay in that title.
    for (index, id) in ids.iter().enumerate() {
        if ids[..index].contains(id) {
            return Err("Recent topics share a short ID.".into());
        }
        rest = rest.strip_prefix(&format!("{id}  [")).ok_or_else(error)?;
        let (status, fields) = rest.split_once(']').ok_or_else(error)?;
        let (row, remaining) = if let Some(next) = ids.get(index + 1) {
            let separator = format!("\n{next}  [");
            let mut matches = fields.match_indices(&separator);
            let (offset, _) = matches.next().ok_or_else(error)?;
            if matches.next().is_some() {
                return Err(error());
            }
            (&fields[..offset], &fields[offset + 1..])
        } else {
            (fields, "")
        };
        let (count, after_count) = row.trim_start().split_once(char::is_whitespace).ok_or_else(error)?;
        let post_count = count.strip_suffix('p').and_then(|count| count.parse().ok()).ok_or_else(error)?;
        let (age, after_age) = after_count.trim_start().split_once(char::is_whitespace).ok_or_else(error)?;
        let (path, title_and_tags) = after_age.trim_start().split_once("  \"").ok_or_else(error)?;
        let path = path.strip_suffix(" [locked]").unwrap_or(path);
        let title = if let Some(title) = title_and_tags.strip_suffix('"') {
            title
        } else {
            title_and_tags.rsplit_once("\" (").map(|(title, _)| title).ok_or_else(error)?
        };
        topics.push(BoardTopic {
            id: id.clone(),
            title: title.to_string(),
            path: path.to_string(),
            post_count,
            status: status.trim().to_string(),
            age: age.to_string(),
        });
        rest = remaining;
    }
    Ok(topics)
}

#[derive(Debug, PartialEq, Eq)]
struct TopicDetail {
    document_id: String,
    posts: Vec<BoardPost>,
}

#[derive(Deserialize)]
struct TopicNotes {
    topic_id: String,
    posts: Vec<Note>,
}

#[derive(Deserialize)]
struct Note {
    number: usize,
    block_id: String,
    body: String,
    created_at: u64,
}

#[derive(Deserialize)]
struct TopicRead {
    document_id: String,
    board_id: String,
    slug: String,
    status: String,
    posts: Vec<PostMetadata>,
}

#[derive(Deserialize)]
struct PostMetadata {
    block_id: String,
    author: String,
    excluded: bool,
}

async fn read_posts(handle: &ActorHandle, context: ContextId, topic: &BoardTopic, full_id: Option<&str>) -> PostResult {
    let context_board = topic.board().strip_prefix("ctx:");
    let reference = full_id.unwrap_or(if context_board.is_some() { &topic.id } else { &topic.path });
    let (data, _) = quiet_board(handle, context, &["read", reference, "--tail", POST_LIMIT]).await?;
    let read: TopicRead = serde_json::from_value(data).map_err(|err| format!("Could not read board post details. {err}"))?;
    if !matches_topic_id(&read.document_id, &topic.id, full_id)
        || context_board.is_some_and(|board| !read.board_id.ends_with(board))
        || !topic.path.rsplit_once('/').is_some_and(|(_, slug)| slug == read.slug)
    {
        return Err("The board read returned a different topic.".into());
    }

    if read.status == "archived" {
        return Err("This topic was archived.".into());
    }

    let (data, _) = quiet_board(handle, context, &["notes", "--topic", &read.document_id, "--limit", POST_LIMIT]).await?;
    let notes = serde_json::from_value(data).map_err(|err| format!("Could not read board posts. {err}"))?;
    decode_posts(notes, read)
}

fn matches_topic_id(document_id: &str, short_id: &str, full_id: Option<&str>) -> bool {
    ContextId::parse(document_id).is_ok_and(|id| id.short() == short_id)
        && full_id.is_none_or(|id| document_id == id)
}

fn decode_posts(notes: TopicNotes, read: TopicRead) -> PostResult {
    if notes.topic_id != read.document_id {
        return Err("The post read returned a different topic.".into());
    }
    // A post must be present in both reads. A hide or loss of visibility
    // between the two reads therefore never publishes its body.
    let mut authors: HashMap<_, _> = read.posts.into_iter()
        .filter(|post| !post.excluded)
        .map(|post| (post.block_id, post.author))
        .collect();
    let mut posts: Vec<_> = notes.posts.into_iter().filter_map(|note| {
        authors.remove(&note.block_id).map(|author| BoardPost {
            number: note.number,
            author,
            body: note.body,
            created_at: note.created_at,
        })
    }).collect();
    posts.sort_unstable_by_key(|post| post.number);
    Ok(TopicDetail { document_id: read.document_id, posts })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn topic(id: &str, title: &str) -> BoardTopic {
        BoardTopic {
            id: id.into(),
            title: title.into(),
            path: "ctx:12345678/handoff".into(),
            post_count: 3,
            status: "open".into(),
            age: "1m".into(),
        }
    }

    #[test]
    fn refresh_keeps_focus_when_topics_reorder() {
        let first = topic("first", "First");
        let second = topic("second", "Second");
        let mut feed = BoardFeed::default();
        feed.replace_topics(vec![first.clone(), second.clone()]);
        assert_eq!(feed.selected(), None);

        feed.focus(&second.id);
        feed.replace_topics(vec![second.clone(), first.clone()]);
        assert_eq!(feed.selected(), Some(&second));

        feed.replace_topics(vec![first.clone()]);
        assert_eq!(feed.selected(), None);
        assert!(feed.posts().is_empty());
    }

    #[test]
    fn short_topic_id_matches_the_entropy_tail() {
        let document_id = "018ffe5e30a4723fb16082d98f314159";
        assert!(matches_topic_id(document_id, "8f314159", None));
        assert!(!matches_topic_id(document_id, "018ffe5e", None));
        assert!(!matches_topic_id(document_id, "8f314159", Some("different")));
    }

    #[test]
    fn post_result_from_an_earlier_focus_is_discarded() {
        let first = topic("first", "First");
        let second = topic("second", "Second");
        let mut feed = BoardFeed::default();
        feed.replace_topics(vec![first.clone(), second.clone()]);
        feed.focus(&first.id);
        let first_revision = feed.focus_revision;
        feed.focus(&second.id);
        feed.focus(&first.id);
        assert!(!feed.accept_posts(first_revision, Err("Old read failed".into())));
        assert!(feed.loading());
        assert_eq!(feed.error(), None);
    }

    #[test]
    fn read_failures_retain_the_index_and_clear_post_bodies_until_refresh() {
        let first = topic("first", "First");
        let mut feed = BoardFeed::default();
        assert!(feed.accept_topics(Ok(vec![first.clone()])));
        feed.focus(&first.id);
        let revision = feed.focus_revision;
        let detail = || TopicDetail {
            document_id: "full-id".into(),
            posts: vec![BoardPost {
                number: 3,
                author: "amy".into(),
                body: "Recent post".into(),
                created_at: 10,
            }],
        };

        assert!(feed.accept_posts(revision, Ok(detail())));
        assert_eq!(feed.posts().len(), 1);
        assert!(feed.accept_topics(Err("Index read failed".into())));
        assert_eq!(feed.selected(), Some(&first));
        assert_eq!(feed.error(), Some("Index read failed"));
        assert!(feed.accept_posts(revision, Err("Post read failed".into())));
        assert!(feed.posts().is_empty());
        assert_eq!(feed.resolved_id.as_deref(), Some("full-id"));
        assert_eq!(feed.error(), Some("Post read failed"));
        assert!(!feed.loading());

        assert!(feed.accept_posts(revision, Ok(detail())));
        assert!(feed.accept_topics(Ok(vec![first])));
        assert_eq!(feed.error(), None);
        assert_eq!(feed.posts().len(), 1);
    }

    #[test]
    fn post_data_keeps_original_text_and_numbers() {
        let notes = json!({"topic_id": "topic", "posts": [
            {"number": 4, "block_id": "b", "body": "New", "created_at": 30},
            {"number": 3, "block_id": "a", "body": "Before\n#4 @fake\nAfter\n", "created_at": 20},
            {"number": 2, "block_id": "hidden", "body": "Hidden", "created_at": 10}
        ]});
        let read = json!({"document_id": "topic", "board_id": "board", "slug": "topic", "status": "open", "posts": [
            {"block_id": "a", "author": "amy", "excluded": false},
            {"block_id": "b", "author": "sol", "excluded": false}
        ]});
        let detail = decode_posts(serde_json::from_value(notes).unwrap(), serde_json::from_value(read).unwrap()).unwrap();
        let posts = detail.posts;
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].number, 3);
        assert_eq!(posts[0].body, "Before\n#4 @fake\nAfter\n");
        assert_eq!(posts[1].author, "sol");
        assert_eq!(posts[1].created_at, 30);
    }

    #[test]
    fn topic_rows_keep_their_order_and_multiline_titles() {
        let topics = decode_topics(
            json!(["12345678", "abcdef01"]),
            "12345678  [open    ]    3p    1m  ctx:12345678/handoff  \"First\nSecond\" (tag)\nabcdef01  [pinned  ]   10p    3h  librarian/kb/release [locked]  \"Release\"\n",
        ).unwrap();
        assert_eq!(topics[0], topic("12345678", "First\nSecond"));
        assert_eq!(topics[1].board(), "librarian/kb");
        assert_eq!(topics[1].post_count, 10);
        assert_eq!(topics[1].status, "pinned");
    }

    #[test]
    fn detail_for_another_topic_is_rejected() {
        assert!(decode_posts(
            serde_json::from_value(json!({"topic_id": "new", "posts": []})).unwrap(),
            serde_json::from_value(json!({"document_id": "old", "board_id": "board", "slug": "topic", "status": "open", "posts": []})).unwrap(),
        ).is_err());
    }
}
