use super::*;
use axum::{
    http::header,
    response::{
        Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::stream;
use machineemu_core::domain::{Instance, Operation, Run};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Read,
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::mpsc;
use utoipa::ToSchema;

const MAX_EVENT_BYTES: usize = 16 * 1024;
const INSTANCE_HISTORY_BYTES: usize = 1024 * 1024;
const GLOBAL_HISTORY_BYTES: usize = 32 * 1024 * 1024;
const HISTORY_EVENTS: usize = 256;
const INSTANCE_SUBSCRIBERS: usize = 8;
const GLOBAL_SUBSCRIBERS: usize = 128;

#[derive(Clone, Serialize, ToSchema)]
pub(super) struct EventBase {
    schema_version: u32,
    instance_id: String,
    run_id: Option<String>,
    timestamp: String,
}

impl EventBase {
    fn new(instance_id: &str, run_id: Option<&str>) -> Self {
        Self {
            schema_version: 1,
            instance_id: instance_id.into(),
            run_id: run_id.map(str::to_owned),
            timestamp: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .expect("RFC 3339 timestamp"),
        }
    }
}

#[derive(Clone, Serialize, ToSchema)]
pub(super) struct SnapshotEvent {
    #[serde(flatten)]
    base: EventBase,
    state: String,
    revision: i64,
    run_status: Option<String>,
    active_operations: Vec<OperationSummary>,
}

#[derive(Clone, Serialize, ToSchema)]
pub(super) struct OperationSummary {
    operation_id: String,
    kind: String,
    status: String,
}

#[derive(Clone, Serialize, ToSchema)]
pub(super) struct StateEvent {
    #[serde(flatten)]
    base: EventBase,
    state: String,
    revision: i64,
    run_status: Option<String>,
    reason: String,
}

#[derive(Clone, Serialize, ToSchema)]
pub(super) struct OperationEvent {
    #[serde(flatten)]
    base: EventBase,
    operation_id: String,
    kind: String,
    status: String,
    result: Option<Value>,
    failure_code: Option<String>,
}

#[derive(Clone, Serialize, ToSchema)]
pub(super) struct QmpEvent {
    #[serde(flatten)]
    base: EventBase,
    name: String,
    fields: Value,
}

#[derive(Clone)]
struct Record {
    sequence: u64,
    order: u64,
    kind: &'static str,
    id: String,
    data: String,
}

impl Record {
    fn bytes(&self) -> usize {
        self.id.len() + self.data.len() + 32
    }

    fn sse(self) -> Event {
        Event::default()
            .event(self.kind)
            .id(self.id)
            .data(self.data)
    }
}

#[derive(Default)]
struct History {
    next_sequence: u64,
    bytes: usize,
    records: VecDeque<Record>,
    subscribers: BTreeMap<u64, mpsc::Sender<Record>>,
}

pub(super) struct EventHub {
    generation: String,
    histories: BTreeMap<String, History>,
    bytes: usize,
    next_order: u64,
    next_subscriber: u64,
}

impl EventHub {
    pub(super) fn new() -> std::io::Result<Self> {
        let mut random = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        Ok(Self {
            generation: random.iter().map(|byte| format!("{byte:02x}")).collect(),
            histories: BTreeMap::new(),
            bytes: 0,
            next_order: 0,
            next_subscriber: 0,
        })
    }

    fn publish(
        &mut self,
        instance_id: &str,
        kind: &'static str,
        data: Value,
        broadcast: bool,
    ) -> bool {
        let Ok(data) = serde_json::to_string(&data) else {
            return false;
        };
        if data.len() > MAX_EVENT_BYTES {
            return false;
        }
        self.next_order += 1;
        let history = self.histories.entry(instance_id.into()).or_default();
        history.next_sequence += 1;
        let record = Record {
            sequence: history.next_sequence,
            order: self.next_order,
            kind,
            id: format!(
                "{instance_id}:{}:{}",
                self.generation, history.next_sequence
            ),
            data,
        };
        if broadcast {
            history
                .subscribers
                .retain(|_, sender| sender.try_send(record.clone()).is_ok());
        }
        let size = record.bytes();
        history.bytes += size;
        self.bytes += size;
        history.records.push_back(record);
        while history.records.len() > HISTORY_EVENTS || history.bytes > INSTANCE_HISTORY_BYTES {
            if let Some(old) = history.records.pop_front() {
                history.bytes -= old.bytes();
                self.bytes -= old.bytes();
            }
        }
        while self.bytes > GLOBAL_HISTORY_BYTES {
            let oldest = self
                .histories
                .iter()
                .filter_map(|(id, history)| {
                    history.records.front().map(|item| (id.clone(), item.order))
                })
                .min_by_key(|(_, order)| *order);
            let Some((id, _)) = oldest else { break };
            if let Some(history) = self.histories.get_mut(&id)
                && let Some(old) = history.records.pop_front()
            {
                history.bytes -= old.bytes();
                self.bytes -= old.bytes();
            }
        }
        true
    }

    fn subscribe(
        &mut self,
        instance_id: &str,
        cursor: Option<&Cursor>,
        snapshot: Value,
    ) -> Result<(u64, VecDeque<Record>, mpsc::Receiver<Record>), StatusCode> {
        let total: usize = self
            .histories
            .values()
            .map(|history| history.subscribers.len())
            .sum();
        let count = self
            .histories
            .get(instance_id)
            .map_or(0, |history| history.subscribers.len());
        if total >= GLOBAL_SUBSCRIBERS || count >= INSTANCE_SUBSCRIBERS {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        let replayable = cursor.is_some_and(|cursor| {
            if cursor.instance_id != instance_id || cursor.generation != self.generation {
                return false;
            }
            let Some(history) = self.histories.get(instance_id) else {
                return false;
            };
            let first = history
                .records
                .front()
                .map_or(history.next_sequence + 1, |r| r.sequence);
            cursor.sequence >= first.saturating_sub(1) && cursor.sequence <= history.next_sequence
        });
        if !replayable && !self.publish(instance_id, "snapshot", snapshot, false) {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let history = self
            .histories
            .get_mut(instance_id)
            .expect("snapshot creates history");
        let initial = if replayable {
            history
                .records
                .iter()
                .filter(|record| record.sequence > cursor.unwrap().sequence)
                .cloned()
                .collect()
        } else {
            history.records.back().cloned().into_iter().collect()
        };
        let (sender, receiver) = mpsc::channel(64);
        self.next_subscriber += 1;
        history.subscribers.insert(self.next_subscriber, sender);
        Ok((self.next_subscriber, initial, receiver))
    }

    fn unsubscribe(&mut self, instance_id: &str, subscriber: u64) {
        if let Some(history) = self.histories.get_mut(instance_id) {
            history.subscribers.remove(&subscriber);
        }
    }

    pub(super) fn close_instance(&mut self, instance_id: &str) {
        if let Some(history) = self.histories.remove(instance_id) {
            self.bytes -= history.bytes;
        }
    }
}

struct Cursor {
    instance_id: String,
    generation: String,
    sequence: u64,
}

impl Cursor {
    fn parse(value: &str) -> Option<Self> {
        let mut fields = value.split(':');
        let instance_id = fields.next()?;
        let generation = fields.next()?;
        let sequence = fields.next()?;
        if fields.next().is_some()
            || Id::new("instance", instance_id).is_err()
            || generation.len() != 32
            || !generation.bytes().all(|byte| byte.is_ascii_hexdigit())
            || sequence.is_empty()
            || !sequence.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let sequence = sequence.parse().ok()?;
        if sequence == 0 {
            return None;
        }
        Some(Self {
            instance_id: instance_id.into(),
            generation: generation.into(),
            sequence,
        })
    }
}

struct Subscription {
    hub: Arc<Mutex<EventHub>>,
    instance_id: String,
    subscriber: u64,
    receiver: mpsc::Receiver<Record>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Ok(mut hub) = self.hub.lock() {
            hub.unsubscribe(&self.instance_id, self.subscriber);
        }
    }
}

pub(super) fn publish_state(
    state: &AppState,
    instance: &Instance,
    run: Option<&Run>,
    reason: &str,
) {
    publish_state_fields(
        state,
        instance,
        run.map(|run| run.run_id.as_str()),
        run.map(|run| run.status.as_str()),
        reason,
    );
}

pub(super) fn publish_state_fields(
    state: &AppState,
    instance: &Instance,
    run_id: Option<&str>,
    run_status: Option<&str>,
    reason: &str,
) {
    let event = StateEvent {
        base: EventBase::new(instance.instance_id.as_str(), run_id),
        state: instance.state.to_string(),
        revision: instance.revision,
        run_status: run_status.map(str::to_owned),
        reason: reason.into(),
    };
    if let Ok(mut hub) = state.events.lock() {
        hub.publish(instance.instance_id.as_str(), "state", json!(event), true);
    }
}

pub(super) fn publish_operation(state: &AppState, operation: &Operation, run_id: Option<&str>) {
    let safe_result = if operation.status == "completed" {
        operation
            .result_json
            .as_deref()
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .and_then(|value| match operation.kind.as_str() {
                "start" => value
                    .get("state")
                    .and_then(Value::as_str)
                    .filter(|state| matches!(*state, "running" | "paused"))
                    .map(|state| json!({"state": state})),
                "snapshot" => value
                    .get("snapshot_id")
                    .and_then(Value::as_str)
                    .filter(|id| Id::new("snapshot", *id).is_ok())
                    .map(|id| json!({"snapshot_id": id})),
                _ => None,
            })
    } else {
        None
    };
    let event = OperationEvent {
        base: EventBase::new(operation.instance_id.as_str(), run_id),
        operation_id: operation.operation_id.as_str().into(),
        kind: operation.kind.clone(),
        status: operation.status.clone(),
        result: safe_result,
        failure_code: (operation.status == "failed").then(|| "operation_failed".into()),
    };
    if let Ok(mut hub) = state.events.lock() {
        hub.publish(
            operation.instance_id.as_str(),
            "operation",
            json!(event),
            true,
        );
    }
}

pub(super) fn publish_qmp(
    state: &AppState,
    instance_id: &str,
    run_id: &str,
    name: &str,
    fields: Value,
) {
    let event = QmpEvent {
        base: EventBase::new(instance_id, Some(run_id)),
        name: name.into(),
        fields,
    };
    if let Ok(mut hub) = state.events.lock() {
        hub.publish(instance_id, "qmp", json!(event), true);
    }
}

pub(super) async fn stream_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let Ok(instance_id) = Id::new("instance", id.clone()) else {
        return event_error(StatusCode::BAD_REQUEST, "invalid_instance_id");
    };
    let cursor = match headers.get("last-event-id") {
        Some(value) => match value.to_str().ok().and_then(Cursor::parse) {
            Some(cursor) => Some(cursor),
            None => return event_error(StatusCode::BAD_REQUEST, "invalid_event_cursor"),
        },
        None => None,
    };
    let prepared = blocking({
        let state = state.clone();
        move || -> Result<_, RuntimeError> {
            let lock = instance_lock(&state, instance_id.as_str())?;
            let _guard = lock.blocking_lock();
            let (instance, run, operations) = {
                let workspace = state
                    .workspace
                    .lock()
                    .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
                (
                    workspace.instance(&instance_id)?,
                    workspace.active_run(&instance_id)?,
                    workspace.active_operations(&instance_id)?,
                )
            };
            let snapshot = SnapshotEvent {
                base: EventBase::new(
                    instance_id.as_str(),
                    run.as_ref().map(|run| run.run_id.as_str()),
                ),
                state: instance.state.to_string(),
                revision: instance.revision,
                run_status: run.map(|run| run.status),
                active_operations: operations
                    .into_iter()
                    .map(|operation| OperationSummary {
                        operation_id: operation.operation_id.as_str().into(),
                        kind: operation.kind,
                        status: operation.status,
                    })
                    .collect(),
            };
            let subscribed = state
                .events
                .lock()
                .map_err(|_| RuntimeError::Process("event hub lock poisoned".into()))?
                .subscribe(instance_id.as_str(), cursor.as_ref(), json!(snapshot));
            Ok(subscribed.map(|(subscriber, initial, receiver)| {
                (
                    instance_id.as_str().to_owned(),
                    subscriber,
                    initial,
                    receiver,
                )
            }))
        }
    })
    .await;
    let (instance_id, subscriber, initial, receiver) = match prepared {
        Ok(Ok(value)) => value,
        Ok(Err(status)) => return event_error(status, "event_stream_unavailable"),
        Err(RuntimeError::NotFound { .. }) => {
            return event_error(StatusCode::NOT_FOUND, "instance_not_found");
        }
        Err(_) => {
            return event_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "event_stream_unavailable",
            );
        }
    };
    let subscription = Subscription {
        hub: state.events.clone(),
        instance_id,
        subscriber,
        receiver,
    };
    let output = stream::unfold(
        (initial, subscription),
        |(mut initial, mut subscription)| async move {
            let record = match initial.pop_front() {
                Some(record) => Some(record),
                None => subscription.receiver.recv().await,
            }?;
            Some((
                Ok::<Event, std::convert::Infallible>(record.sse()),
                (initial, subscription),
            ))
        },
    );
    let mut response = Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-cache, no-transform".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    response
}

fn event_error(status: StatusCode, code: &str) -> Response {
    (status, axum::Json(ErrorBody { error: code.into() })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_and_resync_follow_the_per_instance_cursor() {
        let mut hub = EventHub::new().unwrap();
        let (subscriber, initial, _receiver) = hub
            .subscribe("lab01", None, json!({"state":"created"}))
            .unwrap();
        let first = initial.front().unwrap().clone();
        assert_eq!(first.kind, "snapshot");
        hub.unsubscribe("lab01", subscriber);
        assert!(hub.publish("lab01", "state", json!({"state":"running"}), true));
        let cursor = Cursor::parse(&first.id).unwrap();
        let (_, replay, _) = hub
            .subscribe("lab01", Some(&cursor), json!({"state":"wrong"}))
            .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].kind, "state");
        let future = Cursor {
            sequence: 999,
            ..Cursor::parse(&first.id).unwrap()
        };
        let (_, resync, _) = hub
            .subscribe("lab01", Some(&future), json!({"state":"running"}))
            .unwrap();
        assert_eq!(resync[0].kind, "snapshot");
        let foreign = Cursor {
            instance_id: "other".into(),
            ..Cursor::parse(&first.id).unwrap()
        };
        let (_, resync, _) = hub
            .subscribe("lab01", Some(&foreign), json!({"state":"running"}))
            .unwrap();
        assert_eq!(resync[0].kind, "snapshot");
        for _ in 0..HISTORY_EVENTS + 1 {
            assert!(hub.publish("lab01", "state", json!({"state":"running"}), true));
        }
        let (_, resync, _) = hub
            .subscribe("lab01", Some(&cursor), json!({"state":"running"}))
            .unwrap();
        assert_eq!(resync[0].kind, "snapshot");
    }

    #[test]
    fn subscriber_limits_and_slow_reader_are_bounded() {
        let mut hub = EventHub::new().unwrap();
        let mut receivers = Vec::new();
        for _ in 0..INSTANCE_SUBSCRIBERS {
            let (_, _, receiver) = hub
                .subscribe("lab01", None, json!({"state":"created"}))
                .unwrap();
            receivers.push(receiver);
        }
        assert!(matches!(
            hub.subscribe("lab01", None, json!({})),
            Err(StatusCode::TOO_MANY_REQUESTS)
        ));
        for _ in 0..65 {
            hub.publish("lab01", "state", json!({"state":"running"}), true);
        }
        assert!(hub.histories["lab01"].subscribers.is_empty());
        assert_eq!(receivers[0].len(), 64);
    }

    #[test]
    fn cursor_validation_rejects_bad_shapes() {
        assert!(Cursor::parse("lab01:0123456789abcdef0123456789abcdef:42").is_some());
        for value in [
            "",
            "lab01:broken:1",
            "../other:0123456789abcdef0123456789abcdef:1",
            "lab01:0123456789abcdef0123456789abcdef:-1",
            "lab01:0123456789abcdef0123456789abcdef:0",
            "lab01:0123456789abcdef0123456789abcdef:1:extra",
        ] {
            assert!(Cursor::parse(value).is_none());
        }
    }
}
