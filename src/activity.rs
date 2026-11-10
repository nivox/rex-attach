//! Read-only block inspection and program-reported activity for the browser.
use crate::{domain::Target, protocol::Control};
use anyhow::Result;
use ratatui::style::Color;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Duration,
};
use tokio::sync::{oneshot, watch};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    #[default]
    Unknown,
    Idle,
    Done,
    Working,
    Error,
    Blocked,
}
impl State {
    fn parse(value: &str) -> Self {
        match value {
            "idle" => Self::Idle,
            "done" => Self::Done,
            "working" => Self::Working,
            "error" => Self::Error,
            "blocked" => Self::Blocked,
            _ => Self::Unknown,
        }
    }
    pub fn badge(self) -> &'static str {
        match self {
            Self::Unknown => "",
            Self::Idle => "💤",
            Self::Done => "✅",
            Self::Working => "🔄",
            Self::Error => "❌",
            Self::Blocked => "🙋",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Idle => "idle",
            Self::Done => "done",
            Self::Working => "working",
            Self::Error => "error",
            Self::Blocked => "needs input",
        }
    }
    pub fn color(self) -> Color {
        match self {
            Self::Unknown => Color::DarkGray,
            Self::Idle => Color::Gray,
            Self::Done => Color::Green,
            Self::Working => Color::Cyan,
            Self::Error => Color::Red,
            Self::Blocked => Color::Yellow,
        }
    }
}
pub fn aggregate(states: impl IntoIterator<Item = State>) -> State {
    states.into_iter().max().unwrap_or_default()
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub state: State,
    pub kind: Option<String>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
    pub progress: Option<u64>,
}
fn text(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
impl Record {
    fn parse(value: &Value) -> Self {
        Self {
            state: State::parse(value["state"].as_str().unwrap_or("")),
            kind: text(value, "kind")
                .filter(|kind| matches!(kind.as_str(), "permission" | "question" | "auth")),
            app: text(value, "effective_app").or_else(|| text(value, "app")),
            title: text(value, "title"),
            msg: text(value, "msg"),
            progress: value["progress"].as_u64().filter(|n| *n <= 100),
        }
    }
    pub fn summary(&self) -> String {
        let mut summary = self.state.label().to_owned();
        if self.state == State::Blocked
            && let Some(kind) = &self.kind
        {
            summary.push_str(&format!(" ({kind})"));
        }
        if let Some(progress) = self.progress {
            summary.push_str(&format!(" {progress}%"));
        }
        summary
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub foreground: Option<String>,
    pub loaded: bool,
    pub records: BTreeMap<String, Record>,
    pub status_error: Option<String>,
    pub details_error: Option<String>,
    status_seq: u64,
}
impl Info {
    pub fn state(&self) -> State {
        aggregate(self.records.values().map(|r| r.state))
    }
    pub fn primary(&self) -> Option<&Record> {
        self.records
            .values()
            .min_by_key(|r| std::cmp::Reverse(r.state))
    }
    pub fn messages(&self) -> Vec<String> {
        let mut records: Vec<_> = self
            .records
            .iter()
            .filter(|(_, r)| r.state != State::Unknown)
            .collect();
        records.sort_by_key(|(_, r)| std::cmp::Reverse(r.state));
        records
            .into_iter()
            .map(|(id, r)| {
                let name = r
                    .title
                    .as_deref()
                    .or(r.app.as_deref())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(if id.is_empty() { "program" } else { id });
                // Put the human message first: a short details pane should
                // still show why input is needed before long task labels.
                format!(
                    "{} {} — {}",
                    r.state.badge(),
                    r.msg.clone().unwrap_or_else(|| r.summary()),
                    name
                )
            })
            .collect()
    }
    fn status(&mut self, reply: &Value, seq: u64) {
        if seq < self.status_seq {
            return;
        }
        self.records = reply["records"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| (r["id"].as_str().unwrap_or("").to_owned(), Record::parse(r)))
            .collect();
        self.status_seq = seq;
        self.status_error = None;
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub blocks: HashMap<Target, Info>,
    pub error: Option<String>,
}
impl Snapshot {
    pub fn info(&self, target: &Target) -> Option<&Info> {
        self.blocks.get(target)
    }
    pub fn state(&self, target: &Target) -> State {
        self.info(target).map_or(State::Unknown, Info::state)
    }
    fn event(&mut self, value: &Value) {
        use crate::wire::StatusUpdate;
        let Some((seq, target, update)) = crate::wire::program_status(value) else {
            return;
        };
        let Some(info) = self.blocks.get_mut(&target) else {
            return;
        };
        if seq <= info.status_seq {
            return;
        }
        match update {
            StatusUpdate::Changed { id, record } => {
                info.records.insert(id, Record::parse(&record));
                info.status_seq = seq;
                info.status_error = None;
            }
            StatusUpdate::Removed { ids } => {
                for id in ids {
                    if id.is_empty() {
                        info.records.clear();
                    } else {
                        info.records
                            .retain(|key, _| key != &id && !key.starts_with(&format!("{id}/")));
                    }
                }
                info.status_seq = seq;
            }
            StatusUpdate::Other => {}
        }
    }
    fn buffered_events(&mut self, control: &mut Control) {
        for event in control.take_events() {
            self.event(&event);
        }
    }
}
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub targets: Vec<Target>,
    pub selected: Option<Target>,
}

pub struct Inspector {
    pub snapshot: watch::Receiver<Snapshot>,
    request: watch::Sender<Request>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Inspector {
    pub fn start(socket: &str, request: Request) -> Self {
        let (requests, rx) = watch::channel(request);
        let (tx, snapshot) = watch::channel(Snapshot::default());
        let (stop, cancelled) = oneshot::channel();
        let socket = socket.to_owned();
        let task = tokio::task::spawn_local(async move {
            if let Err(error) = monitor(&socket, rx, &tx, cancelled).await {
                tx.send_modify(|s| s.error = Some(format!("Information unavailable: {error:#}")));
            }
        });
        Self {
            snapshot,
            request: requests,
            stop: Some(stop),
            task: Some(task),
        }
    }
    pub fn update(&self, request: Request) {
        self.request.send_if_modified(|old| {
            if *old == request {
                false
            } else {
                *old = request;
                true
            }
        });
    }
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Inspector {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
async fn status(control: &mut Control, target: &Target, snapshot: &mut Snapshot) {
    let reply = control
        .call(
            "com.superlogical.terminal.program_status",
            json!({"session_id":target.session,"block_id":target.block,"args":{}}),
        )
        .await;
    let seq = control.response_seq();
    let info = snapshot.blocks.entry(target.clone()).or_default();
    match reply {
        Ok(reply) => info.status(&reply, seq),
        Err(error) => {
            info.records.clear();
            info.status_error = Some(format!("Status unavailable: {error:#}"));
        }
    }
    snapshot.buffered_events(control);
}
async fn details(control: &mut Control, target: &Target, snapshot: &mut Snapshot) {
    let mut replies = Vec::new();
    let mut errors = Vec::new();
    for method in ["title", "pwd", "process"] {
        let reply = control
            .call(
                &format!("com.superlogical.terminal.{method}"),
                json!({"session_id":target.session,"block_id":target.block,"args":{}}),
            )
            .await;
        match reply {
            Ok(reply) => replies.push(reply),
            Err(error) => {
                replies.push(Value::Null);
                errors.push(format!("{method}: {error}"));
            }
        }
        snapshot.buffered_events(control);
    }
    let info = snapshot.blocks.entry(target.clone()).or_default();
    info.title = text(&replies[0], "title");
    let foreground = &replies[2]["foreground"];
    let program = ["script", "argv0", "name"]
        .iter()
        .find_map(|key| foreground[*key].as_str().filter(|s| !s.is_empty()));
    info.foreground = program.map(|name| {
        format!(
            "{}{}",
            name.rsplit('/')
                .next()
                .unwrap_or(name)
                .trim_start_matches('-'),
            foreground["pid"]
                .as_i64()
                .map(|pid| format!(" (PID {pid})"))
                .unwrap_or_default()
        )
    });
    info.cwd = text(&replies[1], "pwd")
        .or_else(|| text(foreground, "cwd"))
        .or_else(|| text(&replies[2]["child"], "cwd"));
    info.loaded = true;
    info.details_error = (!errors.is_empty()).then(|| errors.join("; "));
}
async fn title(control: &mut Control, target: &Target, snapshot: &mut Snapshot) {
    let reply = control
        .call(
            "com.superlogical.terminal.title",
            json!({"session_id":target.session,"block_id":target.block,"args":{}}),
        )
        .await;
    snapshot.blocks.entry(target.clone()).or_default().title =
        reply.ok().and_then(|reply| text(&reply, "title"));
    snapshot.buffered_events(control);
}
async fn monitor(
    socket: &str,
    mut requests: watch::Receiver<Request>,
    tx: &watch::Sender<Snapshot>,
    mut cancelled: oneshot::Receiver<()>,
) -> Result<()> {
    let mut control =
        tokio::select! {result=Control::connect(socket)=>result?,_=&mut cancelled=>return Ok(())};
    control.capture_events();
    let mut attached = HashSet::<String>::new();
    let mut snapshot = Snapshot::default();
    let result = tokio::select! {
        result = run(
        &mut control,
        &mut requests,
        tx,
        &mut attached,
        &mut snapshot,
    ) => result,
        // An interrupted RPC cannot be reused. Dropping Control releases all
        // subscriptions on disconnect, including an uncertain session.attach.
        _ = &mut cancelled => return Ok(()),
    };
    for session in attached {
        let _ = control
            .call("session.detach", json!({"session_id":session}))
            .await;
    }
    result
}
async fn run(
    control: &mut Control,
    requests: &mut watch::Receiver<Request>,
    tx: &watch::Sender<Snapshot>,
    attached: &mut HashSet<String>,
    snapshot: &mut Snapshot,
) -> Result<()> {
    let mut refresh = tokio::time::interval(Duration::from_secs(5));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut selected_tick = tokio::time::interval(Duration::from_secs(1));
    selected_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    selected_tick.tick().await;
    refresh.tick().await;
    let mut publish = tokio::time::interval(Duration::from_millis(100));
    publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut request = Request::default();
    let mut reconcile = true;
    loop {
        if reconcile {
            request = requests.borrow_and_update().clone();
            let desired: HashSet<_> = request.targets.iter().map(|t| t.session.clone()).collect();
            for session in attached.difference(&desired).cloned().collect::<Vec<_>>() {
                control
                    .call("session.detach", json!({"session_id":session}))
                    .await?;
                attached.remove(&session);
            }
            for session in desired.difference(attached).cloned().collect::<Vec<_>>() {
                control
                    .call("session.attach", json!({"session_id":session}))
                    .await?;
                attached.insert(session);
            }
            snapshot
                .blocks
                .retain(|target, _| request.targets.contains(target));
            for target in &request.targets {
                if !snapshot.blocks.contains_key(target) {
                    status(control, target, snapshot).await;
                    title(control, target, snapshot).await;
                }
            }
            if let Some(target) = &request.selected {
                details(control, target, snapshot).await;
            }
            snapshot.buffered_events(control);
            reconcile = false;
        }
        tokio::select! {
            _=publish.tick()=> {tx.send_if_modified(|previous|if previous==snapshot{false}else{*previous=snapshot.clone();true});},
            changed=requests.changed()=> {if changed.is_err(){return Ok(());}reconcile=true;},
            _=refresh.tick()=> {for target in &request.targets {status(control,target,snapshot).await; if request.selected.as_ref() != Some(target) {title(control,target,snapshot).await;}}},
            _=selected_tick.tick()=> {if let Some(target)=&request.selected {details(control,target,snapshot).await;}},
            event=control.next_event()=>snapshot.event(&event?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> Target {
        Target {
            session: "session:one".into(),
            block: "block:a".into(),
        }
    }
    fn event(seq: u64, name: &str, payload: Value) -> Value {
        json!({"type":"event","seq":seq,"payload":{"type":"session_event","payload":{"session_id":"session:one","event":{"type":"block_event","payload":{"block_id":"block:a","creator_name":"com.superlogical.terminal","name":name,"payload":payload}}}}})
    }
    #[test]
    fn precedence_reports_attention_without_inventing_activity() {
        assert_eq!(aggregate([]), State::Unknown);
        assert_eq!(aggregate([State::Unknown, State::Unknown]), State::Unknown);
        for (low, high) in [
            (State::Idle, State::Done),
            (State::Done, State::Working),
            (State::Working, State::Error),
            (State::Error, State::Blocked),
        ] {
            assert_eq!(aggregate([low, high]), high);
        }
        let mut info = Info::default();
        info.status(&json!({"records":[{"state":"idle"},{"id":"worker","state":"blocked","kind":"permission","progress":42,"msg":"Approve deployment"}]}),10);
        assert_eq!(info.state(), State::Blocked);
        assert_eq!(
            info.primary().unwrap().summary(),
            "needs input (permission) 42%"
        );
        assert!(info.messages()[0].contains("Approve deployment"));
        info.status(&json!({"records":[]}), 11);
        assert_eq!(info.state(), State::Unknown);
        assert_eq!(
            Record::parse(&json!({"state":"future-state","progress":255})).state,
            State::Unknown
        );
        assert!(
            Record::parse(&json!({"state":"working","progress":255}))
                .progress
                .is_none()
        );
    }
    #[test]
    fn newer_status_events_survive_old_snapshots_and_removals_clear_descendants() {
        let mut snapshot = Snapshot::default();
        snapshot
            .blocks
            .entry(target())
            .or_default()
            .status(&json!({"records":[{"state":"working"}]}), 10);
        snapshot.event(&event(
            12,
            "program_status_changed",
            json!({"id":"task/child","record":{"state":"blocked","msg":"Answer question"}}),
        ));
        assert_eq!(snapshot.state(&target()), State::Blocked);
        snapshot
            .blocks
            .get_mut(&target())
            .unwrap()
            .status(&json!({"records":[{"state":"idle"}]}), 11);
        snapshot.event(&event(
            9,
            "program_status_changed",
            json!({"record":{"state":"done"}}),
        ));
        assert_eq!(snapshot.state(&target()), State::Blocked);
        snapshot.event(&event(
            13,
            "program_status_removed",
            json!({"ids":["task"]}),
        ));
        assert_eq!(snapshot.state(&target()), State::Working);
        snapshot.event(&event(14, "program_status_removed", json!({"ids":[""]})));
        assert_eq!(snapshot.state(&target()), State::Unknown);
    }
    #[tokio::test]
    async fn status_rpc_preserves_events_received_before_its_reply() -> Result<()> {
        use crate::protocol::{Io, Socket, next_json};
        use futures_util::SinkExt;
        use tokio_tungstenite::{
            WebSocketStream,
            tungstenite::{Message, protocol::Role},
        };
        let (a, b) = tokio::io::duplex(65536);
        let a: Box<dyn Io> = Box::new(a);
        let b: Box<dyn Io> = Box::new(b);
        let client: Socket = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        let mut server: Socket = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
        let server_task = async {
            next_json(&mut server).await?;
            server
                .send(Message::text(
                    json!({"type":"welcome","api_revision":11}).to_string(),
                ))
                .await?;
            let request = next_json(&mut server).await?;
            assert_eq!(
                request["method"],
                "com.superlogical.terminal.program_status"
            );
            server
                .send(Message::text(
                    event(
                        12,
                        "program_status_changed",
                        json!({"id":"child","record":{"state":"blocked","msg":"Needs approval"}}),
                    )
                    .to_string(),
                ))
                .await?;
            server.send(Message::text(json!({"type":"response","id":request["id"],"seq":10,"payload":{"records":[{"state":"working"}]}}).to_string())).await?;
            Ok::<_, anyhow::Error>(())
        };
        let client_task = async {
            let mut control = Control::hello(client).await?;
            control.capture_events();
            let mut snapshot = Snapshot::default();
            status(&mut control, &target(), &mut snapshot).await;
            assert_eq!(snapshot.state(&target()), State::Blocked);
            assert_eq!(snapshot.blocks[&target()].records.len(), 2);
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(server_task, client_task)?;
        Ok(())
    }
    #[test]
    fn malformed_status_events_do_not_advance_the_sequence_cutoff() {
        let mut snapshot = Snapshot::default();
        snapshot.blocks.insert(target(), Info::default());
        snapshot.event(&event(
            10,
            "program_status_changed",
            json!({"id":"task","record":{"state":"working"}}),
        ));
        snapshot.event(&event(
            20,
            "program_status_changed",
            json!({"record":{"state":"error"}}),
        ));
        assert_eq!(snapshot.state(&target()), State::Working);
        snapshot.event(&event(
            11,
            "program_status_changed",
            json!({"id":"task","record":{"state":"done"}}),
        ));
        assert_eq!(snapshot.state(&target()), State::Done);
    }
}
