use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::config::Target;
use crate::errors::app_server_error;
use crate::rpc::{Notification, RpcClient, RpcRequestError};
use crate::session::resume_thread_for_action_with_notifications;
use crate::session::{request_direct_input_without_resume, request_with_direct_input_retry};

/// How long the watched turn must stay silent on the live subscription
/// before the fallback poll runs. While notifications for the turn are
/// flowing, no polls are issued at all. Override the default of 3 seconds
/// with `CODEX_TAMER_TURN_POLL_QUIET_SECS` (clamped to 1-300).
const TURN_POLL_QUIET_SECS_DEFAULT: u64 = 3;
const TURN_POLL_QUIET_SECS_ENV: &str = "CODEX_TAMER_TURN_POLL_QUIET_SECS";
const TURN_RESULT_PAGE_LIMIT: u32 = 100;
const MAX_EARLY_NOTIFICATIONS: usize = 10_000;
const MAX_EARLY_NOTIFICATION_BYTES: usize = 16 * 1024 * 1024;
const MAX_ASSISTANT_ITEMS: usize = 4_096;
const MAX_RETAINED_PROGRESS_EVENTS: usize = 10_000;
const MAX_RETAINED_PROGRESS_BYTES: usize = 16 * 1024 * 1024;

fn turn_poll_quiet_duration() -> Duration {
    static QUIET: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *QUIET.get_or_init(|| {
        let seconds = std::env::var(TURN_POLL_QUIET_SECS_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| seconds.clamp(1, 300))
            .unwrap_or(TURN_POLL_QUIET_SECS_DEFAULT);
        Duration::from_secs(seconds)
    })
}

#[derive(Debug)]
pub struct TurnStartOptions {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    pub yolo: bool,
}

#[derive(Debug)]
pub struct StartedTurn {
    pub acceptance: Value,
    initial_events: Vec<Value>,
    pub thread_id: String,
    pub turn_id: String,
    prompt: Option<String>,
    started_after_epoch: Option<i64>,
    early_notifications: Vec<Notification>,
    assistant_seed: AssistantResponses,
}

#[derive(Default)]
struct EarlyNotificationBuffer {
    notifications: Vec<Notification>,
    bytes: usize,
    overflowed: bool,
}

impl EarlyNotificationBuffer {
    fn push(&mut self, notification: Notification) {
        let bytes = notification
            .method
            .len()
            .saturating_add(notification.params.to_string().len());
        if self.notifications.len() < MAX_EARLY_NOTIFICATIONS
            && self.bytes.saturating_add(bytes) <= MAX_EARLY_NOTIFICATION_BYTES
        {
            self.bytes = self.bytes.saturating_add(bytes);
            self.notifications.push(notification);
        } else {
            self.overflowed = true;
        }
    }

    fn clear(&mut self) {
        self.notifications.clear();
        self.bytes = 0;
        self.overflowed = false;
    }
}

#[derive(Default)]
struct JsonByteCounter {
    bytes: usize,
}

impl Write for JsonByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct ProgressEvents {
    values: Vec<Value>,
    retained_bytes: usize,
    retain: bool,
}

impl ProgressEvents {
    fn new(values: Vec<Value>, retain: bool) -> Result<Self> {
        if !retain {
            return Ok(Self {
                values,
                retained_bytes: 0,
                retain,
            });
        }

        let mut events = Self {
            values: Vec::with_capacity(values.len()),
            retained_bytes: 0,
            retain,
        };
        for value in values {
            events.push(value)?;
        }
        Ok(events)
    }

    fn push(&mut self, value: Value) -> Result<()> {
        if self.retain {
            let bytes = serialized_json_bytes(&value)?;
            if self.values.len() >= MAX_RETAINED_PROGRESS_EVENTS
                || self.retained_bytes.saturating_add(bytes) > MAX_RETAINED_PROGRESS_BYTES
            {
                return Err(app_server_error(format!(
                    "progress retention limit exceeded ({MAX_RETAINED_PROGRESS_EVENTS} events or {MAX_RETAINED_PROGRESS_BYTES} bytes)"
                )));
            }
            self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        }
        self.values.push(value);
        Ok(())
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn as_slice(&self) -> &[Value] {
        &self.values
    }

    fn into_values(self) -> Vec<Value> {
        self.values
    }

    fn release_emitted(&mut self) {
        if !self.retain {
            self.values.clear();
            self.retained_bytes = 0;
        }
    }
}

fn serialized_json_bytes(value: &Value) -> Result<usize> {
    let mut counter = JsonByteCounter::default();
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| app_server_error(format!("failed to measure progress event: {error}")))?;
    Ok(counter.bytes)
}

#[derive(Debug)]
pub struct TurnTerminal {
    pub output: Value,
    pub exit_code: i32,
}

#[derive(Debug)]
pub enum TurnWaitOutcome {
    Terminal(TurnTerminal),
    LocalInterrupt { thread_id: String, turn_id: String },
}

#[derive(Debug, Clone)]
struct AssistantResponses {
    items: Vec<AssistantResponse>,
    alias_to_index: HashMap<String, usize>,
    retain_text: bool,
}

/// One agent message of the watched turn.
///
/// Codex app-server identifies the same item differently per surface: live
/// notifications use opaque ids (`msg_<hash>`), while resume snapshots and
/// `thread/turns/list` renumber items (`item-N`). Entries therefore track
/// every id observed for the item and all emitted events carry the canonical
/// (first-seen) id, so downstream consumers never see one item under two ids.
#[derive(Debug, Clone)]
struct AssistantResponse {
    /// Canonical id used in emitted events: the id this item was first seen
    /// under (snapshot id when seeded, live id otherwise).
    item_id: Option<String>,
    /// Live notification id, when it differs from `item_id`.
    live_id: Option<String>,
    /// Persisted snapshot/poll id, when it differs from `item_id`.
    poll_id: Option<String>,
    text: String,
    observed_len: usize,
    observed_hasher: blake3::Hasher,
    /// While `Some`, a live delta stream may still be replaying the seeded
    /// snapshot text from the item start; tracks how many bytes matched.
    replay_cursor: Option<usize>,
}

impl AssistantResponse {
    fn new(item_id: Option<String>) -> Self {
        Self {
            item_id,
            live_id: None,
            poll_id: None,
            text: String::new(),
            observed_len: 0,
            observed_hasher: blake3::Hasher::new(),
            replay_cursor: None,
        }
    }

    fn content_matches(&self, text: &str, retain_text: bool) -> bool {
        if retain_text {
            return self.text == text;
        }
        self.observed_len == text.len()
            && self.observed_hasher.finalize() == blake3::hash(text.as_bytes())
    }

    fn replace_content(&mut self, text: &str, retain_text: bool) {
        self.observed_len = text.len();
        self.observed_hasher = blake3::Hasher::new();
        self.observed_hasher.update(text.as_bytes());
        self.text = if retain_text {
            text.to_string()
        } else {
            String::new()
        };
    }

    fn append_content(&mut self, fragment: &str, retain_text: bool) {
        self.observed_len = self.observed_len.saturating_add(fragment.len());
        self.observed_hasher.update(fragment.as_bytes());
        if retain_text {
            self.text.push_str(fragment);
        }
    }

    fn alias_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for id in [&self.item_id, &self.live_id, &self.poll_id]
            .into_iter()
            .flatten()
        {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }
}

impl Default for AssistantResponses {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            alias_to_index: HashMap::new(),
            retain_text: true,
        }
    }
}

/// The canonical id and full alias set to stamp on an emitted event.
#[derive(Debug, Clone)]
struct AssistantItemIds {
    item_id: Option<String>,
    alias_ids: Vec<String>,
}

impl AssistantResponses {
    fn stop_retaining_text(&mut self) {
        self.retain_text = false;
        for item in &mut self.items {
            item.text = String::new();
        }
    }

    #[cfg(test)]
    fn text_for_item(&self, item_id: Option<&str>) -> Option<&str> {
        self.find_index(item_id)
            .map(|index| self.items[index].text.as_str())
    }

    fn find_index(&self, item_id: Option<&str>) -> Option<usize> {
        match item_id {
            Some(item_id) => self.alias_to_index.get(item_id).copied(),
            None => self.items.iter().position(|item| item.item_id.is_none()),
        }
    }

    fn register_alias(&mut self, index: usize, item_id: &str) {
        self.alias_to_index
            .entry(item_id.to_string())
            .or_insert(index);
    }

    fn push_item(&mut self, item: AssistantResponse) -> Result<usize> {
        if self.items.len() >= MAX_ASSISTANT_ITEMS {
            return Err(app_server_error(format!(
                "assistant item limit exceeded ({MAX_ASSISTANT_ITEMS} items)"
            )));
        }
        let index = self.items.len();
        for alias in item.alias_ids() {
            self.register_alias(index, &alias);
        }
        self.items.push(item);
        Ok(index)
    }

    /// Records that the live stream declared a new item; deltas for ids that
    /// were never declared belong to the item already in progress when the
    /// subscription started (see `resolve_live`).
    fn note_started(&mut self, item_id: &str) -> Result<()> {
        if self.find_index(Some(item_id)).is_none() {
            self.push_item(AssistantResponse::new(Some(item_id.to_string())))?;
        }
        Ok(())
    }

    /// Finds or creates the entry a live delta/completion with `item_id`
    /// refers to.
    fn resolve_live(&mut self, item_id: Option<&str>) -> Result<usize> {
        if let Some(index) = self.find_index(item_id) {
            return Ok(index);
        }
        if let Some(item_id) = item_id {
            // An identified event upgrades a previously anonymous entry.
            if let Some(index) = self
                .items
                .iter()
                .rposition(|item| item.item_id.is_none() && item.live_id.is_none())
            {
                self.items[index].item_id = Some(item_id.to_string());
                self.register_alias(index, item_id);
                return Ok(index);
            }
            // A live id that was never declared via item/started continues
            // the snapshot-seeded item that was in progress at attach time.
            if let Some(index) = self
                .items
                .iter()
                .rposition(|item| item.live_id.is_none() && item.replay_cursor.is_some())
            {
                self.items[index].live_id = Some(item_id.to_string());
                self.register_alias(index, item_id);
                return Ok(index);
            }
        }
        self.push_item(AssistantResponse::new(item_id.map(str::to_string)))
    }

    /// Applies a live delta. Returns the event ids and the fragment to emit,
    /// or `None` when the delta only replayed already-known seeded text.
    fn apply_live_delta(
        &mut self,
        item_id: Option<&str>,
        delta: &str,
    ) -> Result<Option<(AssistantItemIds, String)>> {
        let retain_text = self.retain_text;
        let index = self.resolve_live(item_id)?;
        let item = &mut self.items[index];
        let mut fragment = delta;
        if let Some(cursor) = item.replay_cursor {
            let remaining = item.text.get(cursor..).unwrap_or("");
            if !remaining.is_empty() && remaining.starts_with(delta) {
                let cursor = cursor + delta.len();
                item.replay_cursor = (cursor < item.text.len()).then_some(cursor);
                return Ok(None);
            }
            if !remaining.is_empty() && delta.starts_with(remaining) {
                fragment = &delta[remaining.len()..];
            }
            item.replay_cursor = None;
        }
        item.append_content(fragment, retain_text);
        Ok(Some((
            AssistantItemIds {
                item_id: item.item_id.clone(),
                alias_ids: item.alias_ids(),
            },
            fragment.to_string(),
        )))
    }

    /// Applies an item/completed text. Returns the event ids when the
    /// completion carries content not yet emitted.
    fn complete_live(
        &mut self,
        item_id: Option<&str>,
        text: &str,
    ) -> Result<Option<AssistantItemIds>> {
        let retain_text = self.retain_text;
        let index = self.resolve_live(item_id)?;
        let item = &mut self.items[index];
        let changed = !item.content_matches(text, retain_text);
        item.replace_content(text, retain_text);
        item.replay_cursor = None;
        Ok(changed.then(|| AssistantItemIds {
            item_id: item.item_id.clone(),
            alias_ids: item.alias_ids(),
        }))
    }

    #[cfg(test)]
    fn set_text(&mut self, item_id: Option<&str>, text: &str) -> Result<()> {
        let retain_text = self.retain_text;
        let index = self.resolve_live(item_id)?;
        self.items[index].replace_content(text, retain_text);
        self.items[index].replay_cursor = None;
        Ok(())
    }

    /// Seeds one snapshot item during attach; order of calls must follow the
    /// item order within the turn.
    fn seed_snapshot_item(&mut self, item_id: Option<&str>, text: &str) -> Result<()> {
        let mut item = AssistantResponse::new(item_id.map(str::to_string));
        item.replace_content(text, self.retain_text);
        item.replay_cursor = Some(0);
        self.push_item(item)?;
        Ok(())
    }

    /// Reconciles a polled turn snapshot. Poll items are joined to known
    /// entries by id alias or by position within the turn (both surfaces list
    /// the turn's agent messages in creation order), so an item streamed live
    /// as `msg_<hash>` is not re-emitted when the poll lists it as `item-N`.
    fn sync_from_turn(&mut self, turn: &Value) -> Result<Vec<(AssistantItemIds, String)>> {
        let mut updates = Vec::new();
        let mut position = 0;
        let retain_text = self.retain_text;
        for item in turn["items"].as_array().unwrap_or(&Vec::new()) {
            if item["type"].as_str() != Some("agentMessage") {
                continue;
            }
            let poll_id = item["id"].as_str();
            let text = item["text"].as_str().unwrap_or("");
            let index = self.find_index(poll_id).or_else(|| {
                self.items
                    .get(position)
                    .filter(|item| match (poll_id, item.poll_id.as_deref()) {
                        (Some(_), None) => true,
                        (Some(poll_id), Some(known)) => poll_id == known,
                        (None, _) => false,
                    })
                    .map(|_| position)
            });
            match index {
                Some(index) => {
                    let mut new_poll_alias = None;
                    let item = &mut self.items[index];
                    if let Some(poll_id) = poll_id
                        && item.item_id.as_deref() != Some(poll_id)
                        && item.poll_id.is_none()
                    {
                        item.poll_id = Some(poll_id.to_string());
                        new_poll_alias = Some(poll_id.to_string());
                    }
                    if !text.is_empty() && !item.content_matches(text, retain_text) {
                        item.replace_content(text, retain_text);
                        if item.replay_cursor.is_some() {
                            item.replay_cursor = Some(0);
                        }
                        updates.push((
                            AssistantItemIds {
                                item_id: item.item_id.clone(),
                                alias_ids: item.alias_ids(),
                            },
                            text.to_string(),
                        ));
                    }
                    if let Some(poll_id) = new_poll_alias {
                        self.register_alias(index, &poll_id);
                    }
                }
                None => {
                    let mut entry = AssistantResponse::new(poll_id.map(str::to_string));
                    entry.replace_content(text, retain_text);
                    let ids = AssistantItemIds {
                        item_id: entry.item_id.clone(),
                        alias_ids: entry.alias_ids(),
                    };
                    self.push_item(entry)?;
                    if !text.is_empty() {
                        updates.push((ids, text.to_string()));
                    }
                }
            }
            position += 1;
        }
        Ok(updates)
    }

    fn final_text(&self) -> String {
        self.items
            .iter()
            .filter(|item| !item.text.is_empty())
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn to_json(&self) -> Vec<Value> {
        self.items
            .iter()
            .filter(|item| !item.text.is_empty())
            .map(|item| {
                let mut map = Map::new();
                if let Some(item_id) = &item.item_id {
                    map.insert("itemId".to_string(), json!(item_id));
                }
                map.insert("text".to_string(), json!(item.text));
                Value::Object(map)
            })
            .collect()
    }
}

fn assistant_for_wait(
    mut assistant: AssistantResponses,
    retain_progress: bool,
) -> AssistantResponses {
    if !retain_progress {
        assistant.stop_retaining_text();
    }
    assistant
}

/// Builds the assistant accumulator state implied by a `thread/resume`
/// snapshot, so that turn waiting starts from the same view of the turn the
/// snapshot describes instead of an empty one. Without this, the first poll
/// re-emits the full text of every item already present in the snapshot.
fn assistant_seed_from_thread_snapshot(
    thread: &Value,
    turn_id: &str,
) -> Result<AssistantResponses> {
    let mut assistant = AssistantResponses::default();
    for turn in thread["turns"].as_array().unwrap_or(&Vec::new()) {
        if turn["id"].as_str() != Some(turn_id) {
            continue;
        }
        for item in turn["items"].as_array().unwrap_or(&Vec::new()) {
            if item["type"].as_str() != Some("agentMessage") {
                continue;
            }
            // Empty items are seeded too: positions within the turn align
            // poll snapshots with live entries, so gaps would mismatch them.
            assistant
                .seed_snapshot_item(item["id"].as_str(), item["text"].as_str().unwrap_or(""))?;
        }
    }
    Ok(assistant)
}

/// Drops or trims agent-message deltas that were buffered while the
/// `thread/resume` request was in flight and whose content the resume
/// snapshot already includes. The server generates the snapshot after sending
/// those deltas, so replaying them verbatim duplicates text downstream.
///
/// Within one item the buffered deltas are contiguous, so their concatenation
/// overlaps the snapshot text's tail by exactly the already-included portion.
/// After this pass, every delta that flows downstream is an exact, new
/// continuation of the text emitted so far.
fn reconcile_replayed_deltas(
    assistant: &AssistantResponses,
    thread_id: &str,
    turn_id: &str,
    notifications: Vec<Notification>,
) -> Vec<Notification> {
    let is_replayed_delta = |notification: &Notification| {
        notification.method == "item/agentMessage/delta"
            && notification.params["threadId"] == thread_id
            && notification.params["turnId"] == turn_id
    };

    let mut snapshot_by_text: HashMap<&str, VecDeque<usize>> = HashMap::new();
    let mut snapshot_ids = HashSet::new();
    let mut snapshot_text_by_id = HashMap::new();
    let mut anonymous_snapshot_text = None;
    for (index, item) in assistant.items.iter().enumerate() {
        if item.item_id.is_some() {
            snapshot_by_text
                .entry(item.text.as_str())
                .or_default()
                .push_back(index);
        } else if anonymous_snapshot_text.is_none() {
            anonymous_snapshot_text = Some(item.text.as_str());
        }
        for id in [&item.item_id, &item.live_id, &item.poll_id]
            .into_iter()
            .flatten()
        {
            snapshot_ids.insert(id.as_str());
            snapshot_text_by_id.insert(id.as_str(), item.text.as_str());
        }
    }

    let mut claimed_snapshot_items = HashSet::new();
    let mut aliases = HashMap::new();
    for notification in &notifications {
        if notification.method != "item/completed"
            || notification.params["threadId"] != thread_id
            || notification.params["turnId"] != turn_id
            || notification.params["item"]["type"].as_str() != Some("agentMessage")
        {
            continue;
        }
        let Some(live_id) = notification.params["item"]["id"].as_str() else {
            continue;
        };
        let Some(text) = notification.params["item"]["text"].as_str() else {
            continue;
        };
        let Some(indices) = snapshot_by_text.get_mut(text) else {
            continue;
        };
        let Some(index) = indices.pop_front() else {
            continue;
        };
        let Some(snapshot_id) = assistant.items[index].item_id.clone() else {
            continue;
        };
        claimed_snapshot_items.insert(index);
        aliases.insert(live_id.to_string(), snapshot_id);
    }

    // Live ids that the buffered window itself declares as new items; deltas
    // for undeclared live ids continue the snapshot's in-progress tail item
    // under a different id namespace (live `msg_<hash>` vs snapshot `item-N`).
    let mut started_in_order = Vec::new();
    let mut started_in_buffer = HashSet::new();
    let mut buffered_delta_text = HashMap::<String, String>::new();
    for notification in &notifications {
        if notification.method == "item/started"
            && notification.params["threadId"] == thread_id
            && notification.params["turnId"] == turn_id
            && notification.params["item"]["type"].as_str() == Some("agentMessage")
            && let Some(live_id) = notification.params["item"]["id"].as_str()
            && started_in_buffer.insert(live_id.to_string())
        {
            started_in_order.push(live_id.to_string());
        }
        if is_replayed_delta(notification)
            && let Some(live_id) = notification.params["itemId"].as_str()
            && let Some(delta) = notification.params["delta"].as_str()
        {
            buffered_delta_text
                .entry(live_id.to_string())
                .or_default()
                .push_str(delta);
        }
    }
    let seed_tail = assistant
        .items
        .iter()
        .enumerate()
        .rev()
        .find(|(index, item)| {
            item.replay_cursor.is_some()
                && item.item_id.is_some()
                && !claimed_snapshot_items.contains(index)
        });
    let seed_tail_id = seed_tail.and_then(|(_, item)| item.item_id.clone());
    if let Some((index, snapshot)) = seed_tail {
        for live_id in &started_in_order {
            let Some(replayed) = buffered_delta_text.get(live_id) else {
                continue;
            };
            if !replayed.is_empty() && snapshot.text == *replayed {
                aliases.insert(
                    live_id.clone(),
                    snapshot.item_id.clone().expect("snapshot id checked"),
                );
                claimed_snapshot_items.insert(index);
                break;
            }
        }
    }
    let resolve = |item_id: Option<&str>| -> Option<String> {
        let Some(item_id) = item_id else {
            return seed_tail_id.clone();
        };
        if let Some(snapshot_id) = aliases.get(item_id) {
            return Some(snapshot_id.clone());
        }
        if snapshot_ids.contains(item_id) {
            return Some(item_id.to_string());
        }
        if started_in_buffer.contains(item_id) {
            return Some(item_id.to_string());
        }
        seed_tail_id.clone().or(Some(item_id.to_string()))
    };
    let mut replayed = HashMap::<Option<String>, (String, usize)>::new();
    let mut replay_order = Vec::new();
    for notification in notifications.iter().filter(|n| is_replayed_delta(n)) {
        let item_id = resolve(notification.params["itemId"].as_str());
        let delta = notification.params["delta"].as_str().unwrap_or("");
        if let Some((text, _)) = replayed.get_mut(&item_id) {
            text.push_str(delta);
        } else {
            replay_order.push(item_id.clone());
            replayed.insert(item_id, (delta.to_string(), 0));
        }
    }
    for item_id in &replay_order {
        let known = match item_id.as_deref() {
            Some(item_id) => snapshot_text_by_id.get(item_id).copied().unwrap_or(""),
            None => anonymous_snapshot_text.unwrap_or(""),
        };
        let (text, skip) = replayed.get_mut(item_id).expect("replay key recorded");
        *skip = replayed_prefix_len(known, text);
    }
    if crate::debuglog::enabled() {
        let items = replay_order
            .iter()
            .map(|item_id| {
                let (text, skip) = replayed.get(item_id).expect("replay key recorded");
                json!({
                    "itemId": item_id,
                    "knownLen": match item_id.as_deref() {
                        Some(item_id) => snapshot_text_by_id
                            .get(item_id)
                            .map(|text| text.len())
                            .unwrap_or(0),
                        None => anonymous_snapshot_text.map(str::len).unwrap_or(0),
                    },
                    "replayedLen": text.len(),
                    "trimmedLen": skip,
                })
            })
            .collect::<Vec<_>>();
        crate::debuglog::log(
            "attach-reconcile",
            None,
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "bufferedNotifications": notifications.len(),
                "items": items,
            }),
        );
    }

    let mut consumed = HashMap::<Option<String>, usize>::new();
    let mut out = Vec::with_capacity(notifications.len());
    for mut notification in notifications {
        if matches!(
            notification.method.as_str(),
            "item/started" | "item/completed"
        ) && notification.params["threadId"] == thread_id
            && notification.params["turnId"] == turn_id
            && let Some(live_id) = notification.params["item"]["id"].as_str()
            && let Some(snapshot_id) = aliases.get(live_id)
        {
            notification.params["item"]["id"] = json!(snapshot_id);
        }
        if !is_replayed_delta(&notification) {
            out.push(notification);
            continue;
        }
        let item_id = resolve(notification.params["itemId"].as_str());
        let delta_len = notification.params["delta"].as_str().unwrap_or("").len();
        let skip = replayed.get(&item_id).map(|(_, skip)| *skip).unwrap_or(0);
        let position = consumed.entry(item_id).or_default();
        let start = *position;
        *position = position.saturating_add(delta_len);
        if start.saturating_add(delta_len) <= skip {
            continue;
        }
        if start < skip {
            let trimmed = notification.params["delta"]
                .as_str()
                .map(|delta| delta[skip - start..].to_string())
                .unwrap_or_default();
            notification.params["delta"] = json!(trimmed);
        }
        out.push(notification);
    }
    out
}

/// Longest prefix of `replayed` that is also a suffix of `existing`, measured
/// in bytes at a char boundary of `replayed`.
fn replayed_prefix_len(existing: &str, replayed: &str) -> usize {
    if replayed.is_empty() {
        return 0;
    }

    for _ in 0..2 {
        let candidate = rolling_overlap_candidate(existing, replayed, random_overlap_hash_base());
        if candidate == 0 || existing[existing.len() - candidate..] == replayed[..candidate] {
            return candidate;
        }
    }

    // A keyed rolling-hash collision is extraordinarily unlikely. Preserve
    // exact behavior without allocating an input-sized prefix table if one
    // nevertheless occurs.
    let max = existing.len().min(replayed.len());
    replayed
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(replayed.len()))
        .filter(|length| *length <= max && existing.is_char_boundary(existing.len() - *length))
        .rev()
        .find(|length| existing.ends_with(&replayed[..*length]))
        .unwrap_or(0)
}

fn random_overlap_hash_base() -> u128 {
    let state = std::collections::hash_map::RandomState::new();
    let mut hasher = state.build_hasher();
    hasher.write_u8(0);
    u128::from(hasher.finish()).max(257) | 1
}

fn rolling_overlap_candidate(existing: &str, replayed: &str, base: u128) -> usize {
    let existing_bytes = existing.as_bytes();
    let replayed_bytes = replayed.as_bytes();
    let max = existing_bytes.len().min(replayed_bytes.len());
    let mut prefix_hash = 0_u128;
    let mut suffix_hash = 0_u128;
    let mut power = 1_u128;
    let mut best = 0;

    for length in 1..=max {
        prefix_hash = prefix_hash
            .wrapping_mul(base)
            .wrapping_add(u128::from(replayed_bytes[length - 1]) + 1);
        suffix_hash = (u128::from(existing_bytes[existing_bytes.len() - length]) + 1)
            .wrapping_mul(power)
            .wrapping_add(suffix_hash);
        if prefix_hash == suffix_hash
            && replayed.is_char_boundary(length)
            && existing.is_char_boundary(existing.len() - length)
        {
            best = length;
        }
        power = power.wrapping_mul(base);
    }
    best
}

pub struct AttachTurnOptions {
    pub thread_id: String,
    pub turn_id: String,
    pub yolo: bool,
    pub poll_limit: u32,
    pub timeout: Duration,
    pub retain_progress: bool,
}

pub struct AttachedTurnWaitOptions {
    pub poll_limit: u32,
    pub timeout: Duration,
    pub retain_progress: bool,
}

pub async fn steer_turn(
    target: &Target,
    client: &mut RpcClient,
    thread_id: String,
    turn_id: String,
    prompt: String,
    _yolo: bool,
) -> Result<Value> {
    let params = json!({"threadId": thread_id, "expectedTurnId": turn_id, "input": [{"type": "text", "text": prompt, "textElements": []}]});
    let result =
        request_direct_input_without_resume(client, "turn/steer", params, &thread_id, |_| {})
            .await?;
    let response_turn_id = result["turnId"]
        .as_str()
        .filter(|turn_id| !turn_id.is_empty())
        .ok_or_else(|| app_server_error("turn/steer response missing turnId"))?;
    if response_turn_id != turn_id {
        return Err(app_server_error(format!(
            "turn/steer response turnId `{response_turn_id}` does not match requested turn `{turn_id}`"
        )));
    }
    Ok(
        json!({"server": target.server, "threadId": thread_id, "turnId": response_turn_id, "status": "accepted"}),
    )
}

pub async fn interrupt_turn(
    target: &Target,
    client: &mut RpcClient,
    thread_id: String,
    turn_id: String,
) -> Result<Value> {
    let result = client
        .request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
            |_| {},
        )
        .await?;
    if !result.is_object() {
        return Err(app_server_error(
            "turn/interrupt response must be an object",
        ));
    }
    Ok(
        json!({"server": target.server, "threadId": thread_id, "turnId": turn_id, "status": "accepted"}),
    )
}

pub async fn attach_turn<F>(
    target: &Target,
    client: &mut RpcClient,
    options: AttachTurnOptions,
    mut on_event: F,
) -> Result<TurnWaitOutcome>
where
    F: FnMut(&Value) -> Result<()>,
{
    let deadline = tokio::time::Instant::now()
        .checked_add(options.timeout)
        .ok_or_else(|| app_server_error("turn wait timeout is too large"))?;
    let mut early_notifications = EarlyNotificationBuffer::default();
    let resume = {
        let resume_request = resume_thread_for_action_with_notifications(
            client,
            &options.thread_id,
            options.yolo,
            /*exclude_turns*/ false,
            |notification| {
                early_notifications.push(notification);
            },
        );
        tokio::pin!(resume_request);
        tokio::select! {
            result = &mut resume_request => result?,
            _ = tokio::time::sleep_until(deadline) => {
                return Err(app_server_error(format!(
                    "timed out waiting for turn `{}` to complete",
                    options.turn_id
                )));
            }
            _ = tokio::signal::ctrl_c() => {
                return Ok(TurnWaitOutcome::LocalInterrupt {
                    thread_id: options.thread_id.clone(),
                    turn_id: options.turn_id.clone(),
                });
            }
        }
    };
    if early_notifications.overflowed {
        return Err(app_server_error(format!(
            "thread/resume exceeded the pre-response notification limit ({MAX_EARLY_NOTIFICATIONS} events or {MAX_EARLY_NOTIFICATION_BYTES} bytes)"
        )));
    }
    let thread = resume
        .get("thread")
        .filter(|thread| thread.is_object())
        .ok_or_else(|| app_server_error("thread/resume response missing thread object"))?;
    let turns = thread["turns"]
        .as_array()
        .ok_or_else(|| app_server_error("thread/resume response missing thread.turns array"))?;
    let snapshot_turn = turns
        .iter()
        .find(|turn| turn["id"].as_str() == Some(options.turn_id.as_str()));
    if let Some(turn) = snapshot_turn {
        reject_unknown_turn_status(turn)?;
    }
    let assistant_seed = assistant_seed_from_thread_snapshot(thread, &options.turn_id)?;
    if crate::debuglog::enabled() {
        let items = assistant_seed
            .items
            .iter()
            .map(|item| json!({"itemId": item.item_id, "textLen": item.text.len()}))
            .collect::<Vec<_>>();
        crate::debuglog::log(
            "attach-seed",
            None,
            json!({
                "threadId": options.thread_id,
                "turnId": options.turn_id,
                "items": items,
            }),
        );
    }
    let early_notifications = reconcile_replayed_deltas(
        &assistant_seed,
        &options.thread_id,
        &options.turn_id,
        early_notifications.notifications,
    );
    let attached = json!({
        "type": "attached",
        "server": target.server,
        "threadId": options.thread_id,
        "turnId": options.turn_id,
        "status": "attached"
    });
    on_event(&attached)?;
    let mut initial_events = ProgressEvents::new(
        if options.retain_progress {
            vec![attached.clone()]
        } else {
            Vec::new()
        },
        options.retain_progress,
    )?;
    for response in assistant_seed.to_json() {
        let mut event = response;
        event["type"] = json!("assistantMessage");
        event["server"] = json!(target.server);
        event["threadId"] = json!(options.thread_id);
        event["turnId"] = json!(options.turn_id);
        event["source"] = json!("snapshot");
        if options.retain_progress {
            initial_events.push(event.clone())?;
        }
        on_event(&event)?;
    }
    let assistant_seed = assistant_for_wait(assistant_seed, options.retain_progress);
    if let Some(turn) = snapshot_turn {
        let status = turn_status(turn);
        if matches!(status, "completed" | "failed" | "interrupted") {
            let mut terminal_event = json!({
                "type": status,
                "server": target.server,
                "threadId": options.thread_id,
                "turnId": options.turn_id,
                "status": status,
                "source": "snapshot"
            });
            if !turn["error"].is_null() {
                terminal_event["error"] = turn["error"].clone();
            }
            initial_events.push(terminal_event.clone())?;
            on_event(&terminal_event)?;
            let wait = TurnWaitContext {
                target,
                thread_id: &options.thread_id,
                turn_id: options.turn_id.clone(),
                prompt: None,
                started_after_epoch: None,
                poll_limit: options.poll_limit,
            };
            return Ok(TurnWaitOutcome::Terminal(turn_terminal(
                &wait,
                status,
                &assistant_seed,
                initial_events.as_slice(),
            )));
        }
    }
    let remaining_timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining_timeout.is_zero() {
        return Err(app_server_error(format!(
            "timed out waiting for turn `{}` to complete",
            options.turn_id
        )));
    }
    drop(resume);
    wait_for_attached_turn(
        target,
        client,
        StartedTurn {
            acceptance: attached,
            initial_events: initial_events.into_values(),
            thread_id: options.thread_id,
            turn_id: options.turn_id,
            prompt: None,
            started_after_epoch: None,
            early_notifications,
            assistant_seed,
        },
        AttachedTurnWaitOptions {
            poll_limit: options.poll_limit,
            timeout: remaining_timeout,
            retain_progress: options.retain_progress,
        },
        on_event,
    )
    .await
}

pub async fn read_turn_result(
    target: &Target,
    client: &mut RpcClient,
    thread_id: &str,
    turn_id: &str,
    max_turns: u32,
) -> Result<Value> {
    let mut cursor = None;
    let mut scanned = 0_u32;
    let turn = loop {
        let remaining = max_turns.saturating_sub(scanned);
        if remaining == 0 {
            break None;
        }
        let mut params = json!({
            "threadId": thread_id,
            "limit": remaining.min(TURN_RESULT_PAGE_LIMIT),
            "sortDirection": "desc",
            "itemsView": "full"
        });
        if let Some(cursor) = cursor.as_deref() {
            params["cursor"] = json!(cursor);
        }
        let result = client
            .request("thread/turns/list", params, |_| {})
            .await?;
        let turns = result["data"].as_array().ok_or_else(|| {
            app_server_error("thread/turns/list response missing data array")
        })?;
        let inspected = turns.len().min(remaining as usize);
        if let Some(turn) = turns
            .iter()
            .take(inspected)
            .find(|turn| turn["id"].as_str() == Some(turn_id))
        {
            break Some(turn.clone());
        }
        scanned = scanned.saturating_add(inspected as u32);
        let next_cursor = match result.get("nextCursor") {
            None | Some(Value::Null) => break None,
            Some(Value::String(cursor)) => cursor.as_str(),
            Some(_) => {
                return Err(app_server_error(
                    "thread/turns/list response nextCursor must be a string or null",
                ));
            }
        };
        if inspected == 0 {
            return Err(app_server_error(format!(
                "thread/turns/list returned an empty turn page with a next cursor while searching for turn `{turn_id}`"
            )));
        }
        if cursor.as_deref() == Some(next_cursor) {
            return Err(app_server_error(format!(
                "thread/turns/list repeated cursor `{next_cursor}` while searching for turn `{turn_id}`"
            )));
        }
        cursor = Some(next_cursor.to_string());
    }
    .ok_or_else(|| {
        app_server_error(format!(
            "turn `{turn_id}` was not found after scanning {scanned} of at most {max_turns} recent turns in thread `{thread_id}`"
        ))
    })?;
    reject_unknown_turn_status(&turn)?;
    let mut assistant = AssistantResponses::default();
    assistant.sync_from_turn(&turn)?;
    let status = turn_status(&turn);
    Ok(json!({
        "server": target.server,
        "threadId": thread_id,
        "turnId": turn_id,
        "status": status,
        "assistantResponses": assistant.to_json(),
        "finalAssistantText": assistant.final_text(),
        "turn": turn
    }))
}

pub async fn wait_for_attached_turn<F>(
    target: &Target,
    client: &mut RpcClient,
    started: StartedTurn,
    options: AttachedTurnWaitOptions,
    mut on_event: F,
) -> Result<TurnWaitOutcome>
where
    F: FnMut(&Value) -> Result<()>,
{
    let mut events = ProgressEvents::new(started.initial_events, options.retain_progress)?;
    let mut assistant = assistant_for_wait(started.assistant_seed, options.retain_progress);
    let mut wait = TurnWaitContext {
        target,
        thread_id: &started.thread_id,
        turn_id: started.turn_id.clone(),
        prompt: started.prompt.as_deref(),
        started_after_epoch: started.started_after_epoch,
        poll_limit: options.poll_limit,
    };
    for notification in started.early_notifications {
        let before_len = events.len();
        if let Some(terminal) =
            process_turn_notification(&wait, notification, &mut assistant, &mut events)?
        {
            emit_new_events(&events, before_len, &mut on_event)?;
            return Ok(TurnWaitOutcome::Terminal(terminal));
        }
        emit_new_events(&events, before_len, &mut on_event)?;
        events.release_emitted();
    }

    let mut poll = tokio::time::interval(Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The live subscription is the primary transport; polling is only the
    // fallback for turns whose notifications stop arriving (or never match,
    // e.g. when turn/start returned a temporary turn id).
    let mut last_turn_evidence = std::time::Instant::now();
    let turn_timeout = tokio::time::sleep(options.timeout);
    tokio::pin!(turn_timeout);
    loop {
        tokio::select! {
            _ = &mut turn_timeout => {
                return Err(app_server_error(format!(
                    "timed out waiting for turn `{}` to complete",
                    started.turn_id
                )));
            }
            _ = tokio::signal::ctrl_c() => {
                return Ok(TurnWaitOutcome::LocalInterrupt {
                    thread_id: started.thread_id.clone(),
                    turn_id: wait.turn_id.clone(),
                });
            }
            notification = client.next_notification_or_request() => {
                let notification = notification?;
                if notification_concerns_turn(&wait, &notification) {
                    last_turn_evidence = std::time::Instant::now();
                }
                let before_len = events.len();
                if let Some(terminal) = process_turn_notification(
                    &wait,
                    notification,
                    &mut assistant,
                    &mut events,
                )? {
                    emit_new_events(&events, before_len, &mut on_event)?;
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
                emit_new_events(&events, before_len, &mut on_event)?;
                events.release_emitted();
            }
            _ = poll.tick() => {
                if last_turn_evidence.elapsed() < turn_poll_quiet_duration() {
                    continue;
                }
                let before_len = events.len();
                let terminal = tokio::select! {
                    _ = &mut turn_timeout => {
                        return Err(app_server_error(format!(
                            "timed out waiting for turn `{}` to complete",
                            started.turn_id
                        )));
                    }
                    _ = tokio::signal::ctrl_c() => {
                        return Ok(TurnWaitOutcome::LocalInterrupt {
                            thread_id: started.thread_id.clone(),
                            turn_id: wait.turn_id.clone(),
                        });
                    }
                    terminal = poll_turn_completion(
                        client,
                        &mut wait,
                        &mut assistant,
                        &mut events,
                    ) => terminal?,
                };
                last_turn_evidence = std::time::Instant::now();
                emit_new_events(&events, before_len, &mut on_event)?;
                if let Some(terminal) = terminal {
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
                events.release_emitted();
            }
        }
    }
}

struct TurnWaitContext<'a> {
    target: &'a Target,
    thread_id: &'a str,
    turn_id: String,
    prompt: Option<&'a str>,
    started_after_epoch: Option<i64>,
    poll_limit: u32,
}

/// Whether a notification proves the live subscription still delivers
/// traffic for the watched turn. Notifications for other turns do not count:
/// they leave open the possibility that `wait.turn_id` is a stale or
/// temporary id whose real turn only the fallback poll can re-align.
fn notification_concerns_turn(wait: &TurnWaitContext<'_>, notification: &Notification) -> bool {
    let params = &notification.params;
    params["threadId"] == wait.thread_id
        && (params["turnId"] == wait.turn_id.as_str()
            || params["turn"]["id"] == wait.turn_id.as_str())
}

pub async fn start_turn(
    target: &Target,
    client: &mut RpcClient,
    thread_id: String,
    prompt: String,
    options: TurnStartOptions,
) -> Result<StartedTurn> {
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(thread_id));
    let prompt_for_match = prompt.clone();
    let started_after_epoch = Some(current_epoch_seconds().saturating_sub(1));
    params.insert(
        "input".to_string(),
        json!([{"type": "text", "text": prompt, "textElements": []}]),
    );
    if options.yolo {
        insert_turn_yolo_permissions(&mut params);
    }
    insert_opt(&mut params, "model", options.model);
    if let Some(effort) = options.effort {
        params.insert("effort".to_string(), json!(effort));
    }
    if let Some(tier) = options.service_tier {
        params.insert("serviceTier".to_string(), json!(tier));
    }
    let early_notifications = Arc::new(Mutex::new(EarlyNotificationBuffer::default()));
    let params = Value::Object(params);
    let retry_notifications = early_notifications.clone();
    let captured_notifications = early_notifications.clone();
    let result = request_with_direct_input_retry(
        client,
        "turn/start",
        params,
        &thread_id,
        options.yolo,
        || {
            retry_notifications
                .lock()
                .expect("early notification buffer poisoned")
                .clear();
        },
        |notification| {
            captured_notifications
                .lock()
                .expect("early notification buffer poisoned")
                .push(notification);
        },
    )
    .await?;
    let turn_id = result["turn"]["id"]
        .as_str()
        .ok_or_else(|| app_server_error("turn/start response missing turn.id"))?
        .to_string();
    let acceptance = json!({"type": "accepted", "server": target.server, "threadId": thread_id, "turnId": turn_id, "status": "accepted"});
    let early_notifications = early_notifications
        .lock()
        .expect("early notification buffer poisoned");
    if early_notifications.overflowed {
        return Err(app_server_error(format!(
            "turn/start exceeded the pre-response notification limit ({MAX_EARLY_NOTIFICATIONS} events or {MAX_EARLY_NOTIFICATION_BYTES} bytes)"
        )));
    }
    Ok(StartedTurn {
        initial_events: vec![acceptance.clone()],
        acceptance,
        thread_id,
        turn_id,
        prompt: Some(prompt_for_match),
        started_after_epoch,
        early_notifications: early_notifications.notifications.clone(),
        assistant_seed: AssistantResponses::default(),
    })
}

pub async fn wait_for_turn<F>(
    target: &Target,
    client: &mut RpcClient,
    started: StartedTurn,
    poll_limit: u32,
    timeout: Duration,
    retain_progress: bool,
    mut on_event: F,
) -> Result<TurnWaitOutcome>
where
    F: FnMut(&Value) -> Result<()>,
{
    let mut events = ProgressEvents::new(
        if retain_progress {
            started.initial_events
        } else {
            Vec::new()
        },
        retain_progress,
    )?;
    let mut assistant = assistant_for_wait(started.assistant_seed, retain_progress);
    let mut wait = TurnWaitContext {
        target,
        thread_id: &started.thread_id,
        turn_id: started.turn_id.clone(),
        prompt: started.prompt.as_deref(),
        started_after_epoch: started.started_after_epoch,
        poll_limit,
    };
    for notification in started.early_notifications {
        let before_len = events.len();
        if let Some(terminal) =
            process_turn_notification(&wait, notification, &mut assistant, &mut events)?
        {
            emit_new_events(&events, before_len, &mut on_event)?;
            return Ok(TurnWaitOutcome::Terminal(terminal));
        }
        emit_new_events(&events, before_len, &mut on_event)?;
        events.release_emitted();
    }
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // See wait_for_attached_turn: polls back off while turn notifications
    // are flowing on the live subscription.
    let mut last_turn_evidence = std::time::Instant::now();
    let turn_timeout = tokio::time::sleep(timeout);
    tokio::pin!(turn_timeout);
    loop {
        tokio::select! {
            _ = &mut turn_timeout => {
                return Err(app_server_error(format!(
                    "timed out waiting for turn `{}` to complete",
                    started.turn_id
                )));
            }
            _ = tokio::signal::ctrl_c() => {
                return Ok(TurnWaitOutcome::LocalInterrupt {
                    thread_id: started.thread_id.clone(),
                    turn_id: wait.turn_id.clone(),
                });
            }
            notification = client.next_notification_or_request() => {
                let notification = notification?;
                if notification_concerns_turn(&wait, &notification) {
                    last_turn_evidence = std::time::Instant::now();
                }
                let before_len = events.len();
                if let Some(terminal) = process_turn_notification(
                    &wait,
                    notification,
                    &mut assistant,
                    &mut events,
                )? {
                    emit_new_events(&events, before_len, &mut on_event)?;
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
                emit_new_events(&events, before_len, &mut on_event)?;
                events.release_emitted();
            }
            _ = poll.tick() => {
                if last_turn_evidence.elapsed() < turn_poll_quiet_duration() {
                    continue;
                }
                let before_len = events.len();
                let terminal = tokio::select! {
                    _ = &mut turn_timeout => {
                        return Err(app_server_error(format!(
                            "timed out waiting for turn `{}` to complete",
                            started.turn_id
                        )));
                    }
                    _ = tokio::signal::ctrl_c() => {
                        return Ok(TurnWaitOutcome::LocalInterrupt {
                            thread_id: started.thread_id.clone(),
                            turn_id: wait.turn_id.clone(),
                        });
                    }
                    terminal = poll_turn_completion(
                        client,
                        &mut wait,
                        &mut assistant,
                        &mut events,
                    ) => terminal?,
                };
                last_turn_evidence = std::time::Instant::now();
                emit_new_events(&events, before_len, &mut on_event)?;
                if let Some(terminal) = terminal {
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
                events.release_emitted();
            }
        }
    }
}

async fn poll_turn_completion(
    client: &mut RpcClient,
    wait: &mut TurnWaitContext<'_>,
    assistant: &mut AssistantResponses,
    events: &mut ProgressEvents,
) -> Result<Option<TurnTerminal>> {
    let mut notifications = EarlyNotificationBuffer::default();
    let result = client
        .request(
            "thread/turns/list",
            json!({"threadId": wait.thread_id, "limit": wait.poll_limit, "sortDirection": "desc", "itemsView": "full"}),
            |notification| notifications.push(notification),
        )
        .await;
    for notification in notifications.notifications {
        if let Some(terminal) = process_turn_notification(wait, notification, assistant, events)? {
            return Ok(Some(terminal));
        }
    }
    if notifications.overflowed {
        return Err(app_server_error(format!(
            "thread/turns/list exceeded the pre-response notification limit ({MAX_EARLY_NOTIFICATIONS} events or {MAX_EARLY_NOTIFICATION_BYTES} bytes)"
        )));
    }
    let result = match result {
        Ok(result) => result,
        Err(err) if is_unmaterialized_turn_history_error(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    if !result["data"].is_array() {
        return Err(app_server_error(
            "thread/turns/list response missing data array",
        ));
    }

    let turn = poll_result_turn(wait, &result);
    let Some(turn) = turn else {
        return Ok(None);
    };
    reject_unknown_turn_status(turn)?;
    let status = turn_status(turn);
    let updates = assistant.sync_from_turn(turn)?;
    for (ids, text) in updates {
        let mut event = Map::new();
        event.insert("type".to_string(), json!("progress"));
        event.insert("server".to_string(), json!(wait.target.server));
        event.insert("threadId".to_string(), json!(wait.thread_id));
        event.insert("turnId".to_string(), json!(&wait.turn_id));
        insert_item_ids(&mut event, &ids);
        event.insert("text".to_string(), json!(text));
        event.insert("source".to_string(), json!("poll"));
        events.push(Value::Object(event))?;
    }
    if !matches!(status, "completed" | "failed" | "interrupted") {
        return Ok(None);
    }
    let mut event = json!({"type": status, "server": wait.target.server, "threadId": wait.thread_id, "turnId": &wait.turn_id, "status": status, "source": "poll"});
    if !turn["error"].is_null() {
        event["error"] = turn["error"].clone();
    }
    events.push(event)?;
    Ok(Some(turn_terminal(
        wait,
        status,
        assistant,
        events.as_slice(),
    )))
}

fn is_unmaterialized_turn_history_error(err: &anyhow::Error) -> bool {
    let Some(error) = err.downcast_ref::<RpcRequestError>() else {
        return false;
    };
    error.method == "thread/turns/list"
        && error.error.code == -32600
        && error.error.message.contains("is not materialized yet")
}

fn poll_result_turn<'a>(wait: &mut TurnWaitContext<'_>, result: &'a Value) -> Option<&'a Value> {
    let turns = result["data"].as_array()?;
    if let Some(turn) = turns
        .iter()
        .find(|turn| turn["id"].as_str() == Some(wait.turn_id.as_str()))
    {
        return Some(turn);
    }
    let prompt = wait.prompt?;
    let turn = turns.first()?;
    if !turn_matches_prompt(turn, prompt) || !turn_started_after(turn, wait.started_after_epoch) {
        return None;
    }
    if let Some(turn_id) = turn["id"].as_str() {
        wait.turn_id = turn_id.to_string();
    }
    Some(turn)
}

fn turn_matches_prompt(turn: &Value, prompt: &str) -> bool {
    let Some(items) = turn["items"].as_array() else {
        return false;
    };
    items.iter().any(|item| {
        item["type"].as_str() == Some("userMessage")
            && user_message_text(item).as_deref() == Some(prompt)
    })
}

fn user_message_text(item: &Value) -> Option<String> {
    let content = item["content"].as_array()?;
    Some(
        content
            .iter()
            .filter_map(|input| input["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn turn_started_after(turn: &Value, started_after_epoch: Option<i64>) -> bool {
    let Some(started_after_epoch) = started_after_epoch else {
        return false;
    };
    turn["startedAt"]
        .as_i64()
        .or_else(|| turn["completedAt"].as_i64())
        .is_some_and(|timestamp| timestamp >= started_after_epoch)
}

fn emit_new_events(
    events: &ProgressEvents,
    before_len: usize,
    on_event: &mut impl FnMut(&Value) -> Result<()>,
) -> Result<()> {
    for event in events.as_slice().iter().skip(before_len) {
        on_event(event)?;
    }
    Ok(())
}

fn process_turn_notification(
    wait: &TurnWaitContext<'_>,
    notification: Notification,
    assistant: &mut AssistantResponses,
    events: &mut ProgressEvents,
) -> Result<Option<TurnTerminal>> {
    let Some(event) = turn_event(
        &wait.target.server,
        wait.thread_id,
        &wait.turn_id,
        notification,
        assistant,
    )?
    else {
        return Ok(None);
    };

    let status = event["status"].as_str().map(str::to_string);
    events.push(event)?;
    if !matches!(
        status.as_deref(),
        Some("completed" | "failed" | "interrupted")
    ) {
        return Ok(None);
    }

    let status = status.expect("status checked");
    Ok(Some(turn_terminal(
        wait,
        &status,
        assistant,
        events.as_slice(),
    )))
}

fn turn_terminal(
    wait: &TurnWaitContext<'_>,
    status: &str,
    assistant: &AssistantResponses,
    events: &[Value],
) -> TurnTerminal {
    let final_text = assistant.final_text();
    let mut output = json!({
        "server": wait.target.server,
        "threadId": wait.thread_id,
        "turnId": &wait.turn_id,
        "status": status,
        "progress": events,
        "assistantResponses": assistant.to_json(),
        "finalAssistantText": final_text
    });
    if let Some(error) = events
        .iter()
        .rev()
        .find_map(|event| event.get("error").filter(|error| !error.is_null()))
    {
        output["error"] = error.clone();
    }
    let exit_code = if output["status"].as_str() == Some("completed") {
        0
    } else {
        1
    };
    TurnTerminal { output, exit_code }
}

fn turn_event(
    server: &str,
    thread_id: &str,
    turn_id: &str,
    notification: Notification,
    assistant: &mut AssistantResponses,
) -> Result<Option<Value>> {
    match notification.method.as_str() {
        "item/started"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            if notification.params["item"]["type"].as_str() == Some("agentMessage")
                && let Some(item_id) = notification.params["item"]["id"].as_str()
            {
                assistant.note_started(item_id)?;
            }
            Ok(None)
        }
        "item/agentMessage/delta"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            let delta = notification.params["delta"].as_str().unwrap_or("");
            let item_id = notification.params["itemId"].as_str();
            let Some((ids, fragment)) = assistant.apply_live_delta(item_id, delta)? else {
                return Ok(None);
            };
            let mut event = Map::new();
            event.insert("type".to_string(), json!("progress"));
            event.insert("server".to_string(), json!(server));
            event.insert("threadId".to_string(), json!(thread_id));
            event.insert("turnId".to_string(), json!(turn_id));
            insert_item_ids(&mut event, &ids);
            event.insert("delta".to_string(), json!(fragment));
            Ok(Some(Value::Object(event)))
        }
        "item/completed"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            if notification.params["item"]["type"].as_str() == Some("agentMessage")
                && let Some(text) = notification.params["item"]["text"].as_str()
            {
                let item_id = notification.params["item"]["id"].as_str();
                if let Some(ids) = assistant.complete_live(item_id, text)? {
                    let mut event = Map::new();
                    event.insert("type".to_string(), json!("assistantMessage"));
                    event.insert("server".to_string(), json!(server));
                    event.insert("threadId".to_string(), json!(thread_id));
                    event.insert("turnId".to_string(), json!(turn_id));
                    insert_item_ids(&mut event, &ids);
                    event.insert("text".to_string(), json!(text));
                    return Ok(Some(Value::Object(event)));
                }
            }
            Ok(None)
        }
        "turn/completed"
            if notification.params["threadId"] == thread_id
                && notification.params["turn"]["id"] == turn_id =>
        {
            reject_unknown_turn_status(&notification.params["turn"])?;
            let status = turn_status(&notification.params["turn"]);
            let mut event = json!({"type": status, "server": server, "threadId": thread_id, "turnId": turn_id, "status": status});
            if !notification.params["turn"]["error"].is_null() {
                event["error"] = notification.params["turn"]["error"].clone();
            }
            Ok(Some(event))
        }
        _ => Ok(None),
    }
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), json!(value));
    }
}

/// Stamps the canonical item id plus, when the item is known under several
/// server ids, the full alias list consumers can match against.
fn insert_item_ids(map: &mut Map<String, Value>, ids: &AssistantItemIds) {
    insert_opt(map, "itemId", ids.item_id.clone());
    if ids.alias_ids.len() > 1 {
        map.insert("itemAliases".to_string(), json!(ids.alias_ids));
    }
}

fn insert_turn_yolo_permissions(map: &mut Map<String, Value>) {
    map.insert("approvalPolicy".to_string(), json!("never"));
    map.insert(
        "sandboxPolicy".to_string(),
        json!({"type": "dangerFullAccess"}),
    );
}

fn current_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn turn_status(turn: &Value) -> &'static str {
    match turn["status"].as_str().expect("turn status validated") {
        "completed" => "completed",
        "interrupted" => "interrupted",
        "failed" => "failed",
        _ => "inProgress",
    }
}

fn reject_unknown_turn_status(turn: &Value) -> Result<()> {
    let status = turn["status"]
        .as_str()
        .ok_or_else(|| app_server_error("app-server returned a turn without a string status"))?;
    match status {
        "completed" | "interrupted" | "failed" | "inProgress" | "running" | "pending" => Ok(()),
        _ => Err(app_server_error(format!(
            "app-server returned unrecognized turn status `{status}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::Endpoint;

    fn delta_notification(item_id: &str, delta: &str) -> Notification {
        Notification {
            method: "item/agentMessage/delta".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": item_id,
                "delta": delta
            }),
        }
    }

    fn replayed_delta_texts(notifications: &[Notification]) -> Vec<String> {
        notifications
            .iter()
            .map(|notification| {
                notification.params["delta"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn poll_snapshot_does_not_reemit_live_streamed_item_under_persisted_id() {
        // Codex names the same item `msg_<hash>` in live notifications but
        // `item-N` in thread snapshots; the poll must not re-emit text that
        // already streamed live under the other id.
        let mut assistant = AssistantResponses::default();
        assistant.note_started("msg_a").unwrap();
        assert!(
            assistant
                .apply_live_delta(Some("msg_a"), "The CLI also ")
                .unwrap()
                .is_some()
        );
        assert!(
            assistant
                .apply_live_delta(Some("msg_a"), "exposes this.")
                .unwrap()
                .is_some()
        );
        assert!(
            assistant
                .complete_live(Some("msg_a"), "The CLI also exposes this.")
                .unwrap()
                .is_none()
        );

        let turn = json!({"items": [
            {"id": "item-3", "type": "agentMessage", "text": "The CLI also exposes this."}
        ]});
        assert!(assistant.sync_from_turn(&turn).unwrap().is_empty());
        assert_eq!(
            assistant.text_for_item(Some("item-3")),
            Some("The CLI also exposes this.")
        );
    }

    #[test]
    fn undeclared_live_id_continues_seeded_snapshot_item() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "Partial sn")
            .unwrap();

        // The live stream replays the item from its start under a live id
        // that was never declared via item/started.
        assert!(
            assistant
                .apply_live_delta(Some("msg_b"), "Partial ")
                .unwrap()
                .is_none()
        );
        let (ids, fragment) = assistant
            .apply_live_delta(Some("msg_b"), "snapshot text continues")
            .unwrap()
            .expect("boundary delta carries fresh tail");
        assert_eq!(ids.item_id.as_deref(), Some("item-7"));
        assert!(ids.alias_ids.contains(&"msg_b".to_string()));
        assert_eq!(fragment, "apshot text continues");
        assert_eq!(
            assistant.text_for_item(Some("item-7")),
            Some("Partial snapshot text continues")
        );
    }

    #[test]
    fn declared_live_item_stays_separate_from_seeded_tail() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "Earlier paragraph.")
            .unwrap();
        assistant.note_started("msg_c").unwrap();
        let (ids, fragment) = assistant
            .apply_live_delta(Some("msg_c"), "New paragraph.")
            .unwrap()
            .expect("new item delta emits");
        assert_eq!(ids.item_id.as_deref(), Some("msg_c"));
        assert_eq!(fragment, "New paragraph.");
        assert_eq!(
            assistant.text_for_item(Some("item-7")),
            Some("Earlier paragraph.")
        );

        // The poll lists both items under persisted ids: positional join
        // registers aliases without re-emitting.
        let unchanged = json!({"items": [
            {"id": "item-7", "type": "agentMessage", "text": "Earlier paragraph."},
            {"id": "item-8", "type": "agentMessage", "text": "New paragraph."}
        ]});
        assert!(assistant.sync_from_turn(&unchanged).unwrap().is_empty());

        // Later growth is emitted once, under the live id plus aliases.
        let advanced = json!({"items": [
            {"id": "item-7", "type": "agentMessage", "text": "Earlier paragraph."},
            {"id": "item-8", "type": "agentMessage", "text": "New paragraph. More."}
        ]});
        let updates = assistant.sync_from_turn(&advanced).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0.item_id.as_deref(), Some("msg_c"));
        assert!(updates[0].0.alias_ids.contains(&"item-8".to_string()));
        assert_eq!(updates[0].1, "New paragraph. More.");
    }

    #[test]
    fn reconcile_replayed_deltas_drops_content_already_in_snapshot() {
        // The item streamed as "The full" + " paragraph" + " with live" +
        // " suffix". The resume snapshot was generated after the third delta;
        // the deltas in flight during the resume RPC are buffered and would
        // otherwise replay content the snapshot already includes.
        let mut assistant = AssistantResponses::default();
        assistant
            .set_text(Some("assistant-1"), "The full paragraph with live")
            .unwrap();

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-1", " paragraph"),
                delta_notification("assistant-1", " with live"),
                delta_notification("assistant-1", " suffix"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec![" suffix"]);
    }

    #[test]
    fn reconcile_trims_buffered_deltas_for_undeclared_live_id() {
        // Buffered deltas arrive under a live id while the snapshot seeded
        // the same in-progress item under its persisted id.
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "The full paragraph with live")
            .unwrap();

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("msg_z", " paragraph"),
                delta_notification("msg_z", " with live"),
                delta_notification("msg_z", " suffix"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec![" suffix"]);
    }

    #[test]
    fn reconcile_keeps_buffered_deltas_for_declared_new_item() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "Earlier paragraph.")
            .unwrap();

        let started = Notification {
            method: "item/started".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {"id": "msg_y", "type": "agentMessage"}
            }),
        };
        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![started, delta_notification("msg_y", "New paragraph.")],
        );

        assert_eq!(reconciled.len(), 2);
        assert_eq!(reconciled[1].params["delta"], "New paragraph.");
    }

    #[test]
    fn reconcile_keeps_declared_new_item_that_extends_snapshot_tail_text() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "Hello")
            .unwrap();
        let started = Notification {
            method: "item/started".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {"id": "msg-new", "type": "agentMessage"}
            }),
        };

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![started, delta_notification("msg-new", "Hello world")],
        );

        assert_eq!(reconciled.len(), 2);
        assert_eq!(reconciled[0].params["item"]["id"], "msg-new");
        assert_eq!(reconciled[1].params["delta"], "Hello world");
        for notification in reconciled {
            let _ = turn_event("work", "thread-1", "turn-1", notification, &mut assistant)
                .expect("valid event");
        }
        assert_eq!(assistant.items.len(), 2);
        assert_eq!(assistant.final_text(), "Hello\nHello world");
    }

    #[test]
    fn reconcile_handles_the_max_notification_window_without_nested_scans() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "snapshot tail")
            .unwrap();
        let mut notifications = Vec::with_capacity(MAX_EARLY_NOTIFICATIONS);
        for index in 0..(MAX_EARLY_NOTIFICATIONS / 2) {
            let item_id = format!("msg-{index}");
            notifications.push(Notification {
                method: "item/started".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {"id": item_id, "type": "agentMessage"}
                }),
            });
            notifications.push(delta_notification(
                &format!("msg-{index}"),
                &format!("new response {index}"),
            ));
        }

        let reconciled = reconcile_replayed_deltas(&assistant, "thread-1", "turn-1", notifications);

        assert_eq!(reconciled.len(), MAX_EARLY_NOTIFICATIONS);
        assert_eq!(reconciled[0].params["item"]["id"], "msg-0");
        assert_eq!(
            reconciled.last().expect("last delta").params["delta"],
            format!("new response {}", MAX_EARLY_NOTIFICATIONS / 2 - 1)
        );
    }

    #[test]
    fn replay_overlap_handles_one_max_sized_buffered_item() {
        let text = "x".repeat(MAX_EARLY_NOTIFICATION_BYTES);

        assert_eq!(replayed_prefix_len(&text, &text), text.len());
    }

    #[test]
    fn reconcile_started_replay_already_in_snapshot_tail() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "Partial response")
            .unwrap();

        let started = Notification {
            method: "item/started".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {"id": "msg_y", "type": "agentMessage"}
            }),
        };
        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                started,
                delta_notification("msg_y", "Partial "),
                delta_notification("msg_y", "response"),
            ],
        );

        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].method, "item/started");
        assert_eq!(reconciled[0].params["item"]["id"], "item-7");

        let mut live_assistant = assistant.clone();
        for notification in reconciled {
            assert!(
                turn_event(
                    "work",
                    "thread-1",
                    "turn-1",
                    notification,
                    &mut live_assistant,
                )
                .expect("valid event")
                .is_none()
            );
        }
        assert_eq!(live_assistant.items.len(), 1);
        assert_eq!(live_assistant.final_text(), "Partial response");
    }

    #[test]
    fn reconcile_replayed_deltas_drops_replay_from_item_start() {
        let mut assistant = AssistantResponses::default();
        assistant.set_text(Some("assistant-1"), "Hello").unwrap();

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-1", "Hel"),
                delta_notification("assistant-1", "lo"),
                delta_notification("assistant-1", " world"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec![" world"]);
    }

    #[test]
    fn reconcile_replayed_deltas_trims_delta_spanning_snapshot_boundary() {
        let mut assistant = AssistantResponses::default();
        assistant.set_text(Some("assistant-1"), "AB").unwrap();

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-1", "A"),
                delta_notification("assistant-1", "BC"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec!["C"]);
    }

    #[test]
    fn reconcile_replayed_deltas_trims_at_multibyte_boundaries() {
        let mut assistant = AssistantResponses::default();
        assistant.set_text(Some("assistant-1"), "héllo wö").unwrap();

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-1", "héllo"),
                delta_notification("assistant-1", " wörld"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec!["rld"]);
    }

    #[test]
    fn reconcile_replayed_deltas_keeps_unknown_items_and_other_threads() {
        let mut assistant = AssistantResponses::default();
        assistant
            .set_text(Some("assistant-1"), "known text")
            .unwrap();

        let other_thread = Notification {
            method: "item/agentMessage/delta".to_string(),
            params: json!({
                "threadId": "thread-2",
                "turnId": "turn-1",
                "itemId": "assistant-1",
                "delta": "known text"
            }),
        };
        let completed = Notification {
            method: "item/completed".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {"id": "assistant-1", "type": "agentMessage", "text": "known text"}
            }),
        };
        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-2", "new item text"),
                other_thread,
                completed,
            ],
        );

        assert_eq!(reconciled.len(), 3);
        assert_eq!(reconciled[0].params["delta"], "new item text");
        assert_eq!(reconciled[1].params["threadId"], "thread-2");
        assert_eq!(reconciled[1].params["delta"], "known text");
        assert_eq!(reconciled[2].method, "item/completed");
    }

    #[test]
    fn assistant_seed_from_snapshot_suppresses_poll_rebroadcast() {
        let thread = json!({
            "turns": [
                {
                    "id": "turn-1",
                    "items": [
                        {"id": "user-1", "type": "userMessage", "content": [{"text": "go"}]},
                        {"id": "assistant-1", "type": "agentMessage", "text": "First paragraph"},
                        {"id": "assistant-2", "type": "agentMessage", "text": "Second part"}
                    ]
                },
                {
                    "id": "turn-0",
                    "items": [
                        {"id": "assistant-0", "type": "agentMessage", "text": "Older turn"}
                    ]
                }
            ]
        });
        let mut assistant = assistant_seed_from_thread_snapshot(&thread, "turn-1").unwrap();
        assert_eq!(
            assistant.text_for_item(Some("assistant-1")),
            Some("First paragraph")
        );
        assert_eq!(assistant.text_for_item(Some("assistant-0")), None);

        // Polling the same state right after attaching must not re-emit the
        // items the snapshot already delivered.
        let unchanged = json!({
            "id": "turn-1",
            "items": [
                {"id": "assistant-1", "type": "agentMessage", "text": "First paragraph"},
                {"id": "assistant-2", "type": "agentMessage", "text": "Second part"}
            ]
        });
        assert!(assistant.sync_from_turn(&unchanged).unwrap().is_empty());

        let advanced = json!({
            "id": "turn-1",
            "items": [
                {"id": "assistant-1", "type": "agentMessage", "text": "First paragraph"},
                {"id": "assistant-2", "type": "agentMessage", "text": "Second part grew"}
            ]
        });
        let updates = assistant.sync_from_turn(&advanced).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0.item_id.as_deref(), Some("assistant-2"));
        assert_eq!(updates[0].1, "Second part grew");
    }

    #[test]
    fn turn_terminal_preserves_multiple_assistant_item_responses() {
        let target = Target {
            server: "work".to_string(),
            endpoint: Endpoint::Unix {
                path: PathBuf::from("/tmp/mock.sock"),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let wait = TurnWaitContext {
            target: &target,
            thread_id: "thread-1",
            turn_id: "turn-1".to_string(),
            prompt: None,
            started_after_epoch: None,
            poll_limit: 50,
        };
        let mut assistant = AssistantResponses::default();
        let mut events = ProgressEvents::new(Vec::new(), true).unwrap();

        for notification in [
            Notification {
                method: "item/agentMessage/delta".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "assistant-1",
                    "delta": "first"
                }),
            },
            Notification {
                method: "item/agentMessage/delta".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "assistant-1",
                    "delta": " response"
                }),
            },
            Notification {
                method: "item/agentMessage/delta".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "assistant-2",
                    "delta": "second"
                }),
            },
            Notification {
                method: "item/completed".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "id": "assistant-2",
                        "type": "agentMessage",
                        "text": "second corrected"
                    }
                }),
            },
        ] {
            assert!(
                process_turn_notification(&wait, notification, &mut assistant, &mut events)
                    .unwrap()
                    .is_none()
            );
        }

        let terminal = process_turn_notification(
            &wait,
            Notification {
                method: "turn/completed".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turn": {"id": "turn-1", "status": "completed", "items": []}
                }),
            },
            &mut assistant,
            &mut events,
        )
        .unwrap()
        .expect("terminal turn");

        assert_eq!(
            terminal.output["finalAssistantText"],
            "first response\nsecond corrected"
        );
        assert_eq!(
            terminal.output["assistantResponses"],
            json!([
                {"itemId": "assistant-1", "text": "first response"},
                {"itemId": "assistant-2", "text": "second corrected"}
            ])
        );
        assert_eq!(terminal.output["progress"][0]["itemId"], "assistant-1");
        assert_eq!(terminal.output["progress"][2]["itemId"], "assistant-2");
        assert_eq!(terminal.output["progress"][3]["type"], "assistantMessage");
        assert_eq!(terminal.output["progress"][3]["itemId"], "assistant-2");
    }

    #[test]
    fn assistant_response_adopts_item_id_for_provisional_delta() {
        let target = Target {
            server: "work".to_string(),
            endpoint: Endpoint::Unix {
                path: PathBuf::from("/tmp/mock.sock"),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let wait = TurnWaitContext {
            target: &target,
            thread_id: "thread-1",
            turn_id: "turn-1".to_string(),
            prompt: None,
            started_after_epoch: None,
            poll_limit: 50,
        };
        let mut assistant = AssistantResponses::default();
        let mut events = ProgressEvents::new(Vec::new(), true).unwrap();

        process_turn_notification(
            &wait,
            Notification {
                method: "item/agentMessage/delta".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "delta": "draft"
                }),
            },
            &mut assistant,
            &mut events,
        )
        .unwrap();
        process_turn_notification(
            &wait,
            Notification {
                method: "item/completed".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "id": "assistant-1",
                        "type": "agentMessage",
                        "text": "draft final"
                    }
                }),
            },
            &mut assistant,
            &mut events,
        )
        .unwrap();

        assert_eq!(assistant.final_text(), "draft final");
        assert_eq!(
            assistant.to_json(),
            vec![json!({"itemId": "assistant-1", "text": "draft final"})]
        );
    }

    #[test]
    fn assistant_response_sync_from_turn_reports_changes_once() {
        let mut assistant = AssistantResponses::default();
        let turn = json!({
            "items": [
                {
                    "id": "assistant-1",
                    "type": "agentMessage",
                    "text": "current active text"
                }
            ]
        });

        let updates = assistant.sync_from_turn(&turn).unwrap();

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0.item_id.as_deref(), Some("assistant-1"));
        assert_eq!(updates[0].1, "current active text");
        assert_eq!(assistant.final_text(), "current active text");
        assert!(assistant.sync_from_turn(&turn).unwrap().is_empty());
    }

    #[test]
    fn completion_under_an_undeclared_live_id_does_not_repeat_seeded_text() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "already complete")
            .unwrap();

        assert!(
            assistant
                .complete_live(Some("msg-live"), "already complete")
                .unwrap()
                .is_none()
        );
        assert_eq!(assistant.final_text(), "already complete");
    }

    #[test]
    fn buffered_started_and_completed_events_alias_to_the_snapshot_item() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "already complete")
            .unwrap();
        let notifications = vec![
            Notification {
                method: "item/started".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {"id": "msg-live", "type": "agentMessage"}
                }),
            },
            Notification {
                method: "item/completed".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "id": "msg-live",
                        "type": "agentMessage",
                        "text": "already complete"
                    }
                }),
            },
        ];
        let notifications =
            reconcile_replayed_deltas(&assistant, "thread-1", "turn-1", notifications);

        for notification in notifications {
            assert!(
                turn_event("work", "thread-1", "turn-1", notification, &mut assistant,)
                    .expect("valid event")
                    .is_none()
            );
        }
        assert_eq!(assistant.final_text(), "already complete");
        assert_eq!(assistant.items.len(), 1);
    }

    #[test]
    fn streamed_event_history_is_released_after_emission() {
        let mut events = ProgressEvents::new(
            vec![json!({"type": "progress", "delta": "chunk"}); 1_000],
            false,
        )
        .unwrap();
        events.release_emitted();
        assert!(events.as_slice().is_empty());

        let mut events = ProgressEvents::new(Vec::new(), true).unwrap();
        events
            .push(json!({"type": "progress", "delta": "kept"}))
            .unwrap();
        events.release_emitted();
        assert_eq!(events.as_slice().len(), 1);
    }

    #[test]
    fn assistant_registry_indexes_aliases_and_rejects_items_past_the_limit() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-0"), "partial")
            .unwrap();
        assistant
            .apply_live_delta(Some("msg-live"), " continuation")
            .unwrap();

        assert_eq!(assistant.alias_to_index.get("item-0"), Some(&0));
        assert_eq!(assistant.alias_to_index.get("msg-live"), Some(&0));

        for index in 1..MAX_ASSISTANT_ITEMS {
            assistant.note_started(&format!("item-{index}")).unwrap();
        }
        let error = assistant.note_started("item-overflow").unwrap_err();

        assert!(error.to_string().contains("assistant item limit"));
        assert_eq!(assistant.items.len(), MAX_ASSISTANT_ITEMS);
        assert_eq!(assistant.alias_to_index.len(), MAX_ASSISTANT_ITEMS + 1);
    }

    #[test]
    fn retained_progress_rejects_events_past_the_count_limit() {
        let mut events = ProgressEvents::new(Vec::new(), true).unwrap();
        for _ in 0..MAX_RETAINED_PROGRESS_EVENTS {
            events.push(json!({"type": "progress"})).unwrap();
        }

        let error = events
            .push(json!({"type": "progress", "delta": "overflow"}))
            .unwrap_err();

        assert!(error.to_string().contains("progress retention limit"));
        assert_eq!(events.as_slice().len(), MAX_RETAINED_PROGRESS_EVENTS);
    }

    #[test]
    fn retained_progress_rejects_aggregate_bytes_past_the_limit() {
        let mut events = ProgressEvents::new(Vec::new(), true).unwrap();
        let oversized = json!({
            "type": "progress",
            "delta": "x".repeat(MAX_RETAINED_PROGRESS_BYTES)
        });

        let error = events.push(oversized).unwrap_err();

        assert!(error.to_string().contains("progress retention limit"));
        assert!(events.as_slice().is_empty());
    }

    #[test]
    fn non_retaining_assistant_releases_text_without_rebroadcasting_completion() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), &"x".repeat(1_000_000))
            .unwrap();
        let replay = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![delta_notification("item-7", &"x".repeat(1_000_000))],
        );
        assert!(replay.is_empty());
        let mut assistant = assistant_for_wait(assistant, false);

        assert!(assistant.items[0].text.is_empty());
        assert_eq!(assistant.items[0].text.capacity(), 0);
        assert!(
            assistant
                .apply_live_delta(Some("item-7"), " continuation")
                .unwrap()
                .is_some()
        );
        assert!(assistant.items[0].text.is_empty());
        assert!(assistant
            .sync_from_turn(&json!({"items": [
                {"id": "item-7", "type": "agentMessage", "text": format!("{} continuation", "x".repeat(1_000_000))}
            ]}))
            .unwrap()
            .is_empty());
        assert!(
            assistant
                .complete_live(
                    Some("item-7"),
                    &format!("{} continuation", "x".repeat(1_000_000)),
                )
                .unwrap()
                .is_none()
        );
        assert!(assistant.items[0].text.is_empty());
    }

    #[test]
    fn non_retaining_assistant_aliases_late_live_id_to_snapshot_tail() {
        let mut assistant = AssistantResponses::default();
        assistant
            .seed_snapshot_item(Some("item-7"), "partial response")
            .unwrap();
        let mut assistant = assistant_for_wait(assistant, false);

        let (ids, fragment) = assistant
            .apply_live_delta(Some("msg-live"), " continued")
            .unwrap()
            .expect("new suffix emits");

        assert_eq!(ids.item_id.as_deref(), Some("item-7"));
        assert!(ids.alias_ids.contains(&"msg-live".to_string()));
        assert_eq!(fragment, " continued");
        assert_eq!(assistant.items.len(), 1);
        assert!(
            assistant
                .complete_live(Some("msg-live"), "partial response continued")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn early_notification_buffer_enforces_its_byte_limit() {
        let mut buffer = EarlyNotificationBuffer {
            bytes: MAX_EARLY_NOTIFICATION_BYTES,
            ..EarlyNotificationBuffer::default()
        };
        buffer.push(Notification {
            method: "turn/completed".to_string(),
            params: json!({}),
        });

        assert!(buffer.overflowed);
        assert!(buffer.notifications.is_empty());
    }

    #[test]
    fn poll_result_turn_adopts_persisted_turn_id_by_prompt_when_start_id_is_absent() {
        let target = Target {
            server: "work".to_string(),
            endpoint: Endpoint::Unix {
                path: PathBuf::from("/tmp/mock.sock"),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut wait = TurnWaitContext {
            target: &target,
            thread_id: "thread-1",
            turn_id: "returned-id".to_string(),
            prompt: Some("Reply with exactly: ok"),
            started_after_epoch: Some(1_700_000_000),
            poll_limit: 50,
        };
        let result = json!({
            "data": [
                {
                    "id": "persisted-id",
                    "status": "completed",
                    "startedAt": 1_700_000_001_i64,
                    "items": [
                        {
                            "id": "item-user",
                            "type": "userMessage",
                            "content": [{"type": "text", "text": "Reply with exactly: ok"}]
                        },
                        {
                            "id": "item-agent",
                            "type": "agentMessage",
                            "text": "ok"
                        }
                    ]
                }
            ]
        });

        let turn = poll_result_turn(&mut wait, &result).expect("aliased turn");

        assert_eq!(turn["id"], "persisted-id");
        assert_eq!(wait.turn_id, "persisted-id");
    }

    #[test]
    fn poll_result_turn_does_not_alias_to_older_repeated_prompt() {
        let target = Target {
            server: "work".to_string(),
            endpoint: Endpoint::Unix {
                path: PathBuf::from("/tmp/mock.sock"),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut wait = TurnWaitContext {
            target: &target,
            thread_id: "thread-1",
            turn_id: "returned-id".to_string(),
            prompt: Some("repeat prompt"),
            started_after_epoch: Some(1_700_000_000),
            poll_limit: 50,
        };
        let result = json!({
            "data": [
                {
                    "id": "newest-other-turn",
                    "status": "completed",
                    "startedAt": 1_700_000_010_i64,
                    "items": [
                        {
                            "id": "item-user-new",
                            "type": "userMessage",
                            "content": [{"type": "text", "text": "different prompt"}]
                        }
                    ]
                },
                {
                    "id": "older-repeated-turn",
                    "status": "completed",
                    "startedAt": 1_699_999_000_i64,
                    "items": [
                        {
                            "id": "item-user-old",
                            "type": "userMessage",
                            "content": [{"type": "text", "text": "repeat prompt"}]
                        }
                    ]
                }
            ]
        });

        assert!(poll_result_turn(&mut wait, &result).is_none());
        assert_eq!(wait.turn_id, "returned-id");
    }
}
