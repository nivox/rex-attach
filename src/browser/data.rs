use super::model::*;
use crate::protocol::Control;
use anyhow::{Context, Result};
use serde_json::{Value, json};
pub(super) async fn load(control: &mut Control, scope: Option<&str>) -> Result<Vec<Session>> {
    // Older servers may not expose creator metadata; ID defaults still work.
    let creators = control
        .call("block_creator.list", json!({}))
        .await
        .unwrap_or(Value::Null);
    let reply = control.call("session.list", json!({})).await?;
    let mut sessions = Vec::new();
    let reply: crate::wire::SessionList = crate::wire::decode(&reply, "session.list response")?;
    for session in &reply.sessions {
        let metadata: crate::wire::Session = crate::wire::decode(session, "session metadata")?;
        let id = metadata.session_id.as_str();
        if scope.is_some_and(|scope| scope != id) {
            continue;
        }
        let mut tree = crate::session::with_session(control, id, async |control, attached| {
            let blocks = control
                .call("session.list_blocks", json!({"session_id":id}))
                .await?;
            {
                let blocks: crate::wire::BlockList =
                    crate::wire::decode(&blocks, "session.list_blocks response")?;
                session_tree(session, &attached["view"], &blocks.blocks)
            }
        })
        .await?;
        for block in tree
            .windows
            .iter_mut()
            .flat_map(|window| &mut window.blocks)
        {
            block.recognize_flavor_default(&creators);
        }
        sessions.push(tree);
    }
    Ok(sessions)
}

use super::dialog::{Dialog, Operation};
use crate::{actions, task::Task};
use tokio::sync::mpsc;

pub(super) struct Loaded {
    pub sessions: Vec<Session>,
    pub created: Option<String>,
}
enum Command {
    Refresh,
    Mutate(Dialog),
}

/// Exactly one in-flight operation; repeated refresh requests are coalesced.
/// Scope is immutable for this worker. Results are applied by ID to current UI
/// selection, never to the selection that happened to start a refresh.
pub(super) struct Data {
    requests: mpsc::Sender<Command>,
    replies: mpsc::Receiver<Result<Loaded, String>>,
    task: Task,
    busy: bool,
}
impl Data {
    pub fn start(socket: &str, scope: Option<&str>) -> Self {
        let socket = socket.to_owned();
        Self::connect(async move { Control::connect(&socket).await }, scope)
    }
    fn connect(
        connection: impl std::future::Future<Output = Result<Control>> + 'static,
        scope: Option<&str>,
    ) -> Self {
        let (requests, rx) = mpsc::channel(1);
        let (tx, replies) = mpsc::channel(1);
        let scope = scope.map(str::to_owned);
        let task = Task::spawn(async move {
            serve(connection, scope.as_deref(), rx, tx).await;
        });
        let mut data = Self {
            requests,
            replies,
            task,
            busy: false,
        };
        data.refresh();
        data
    }
    pub fn refresh(&mut self) {
        if !self.busy && self.requests.try_send(Command::Refresh).is_ok() {
            self.busy = true;
        }
    }
    pub fn mutate(&mut self, dialog: Dialog) -> Option<Dialog> {
        if self.busy {
            return Some(dialog);
        }
        match self.requests.try_send(Command::Mutate(dialog)) {
            Ok(()) => {
                self.busy = true;
                None
            }
            Err(error) => match error.into_inner() {
                Command::Mutate(dialog) => Some(dialog),
                Command::Refresh => unreachable!("submitted a mutation"),
            },
        }
    }
    pub async fn changed(&mut self) -> Option<Result<Loaded, String>> {
        let reply = self.replies.recv().await;
        self.busy = false;
        reply
    }
    pub async fn stop(self) {
        self.task.stop().await;
    }
}
async fn serve(
    connection: impl std::future::Future<Output = Result<Control>>,
    scope: Option<&str>,
    mut rx: mpsc::Receiver<Command>,
    tx: mpsc::Sender<Result<Loaded, String>>,
) {
    let mut control = match connection.await {
        Ok(control) => control,
        Err(error) => {
            let _ = tx
                .send(Err(format!("Browser unavailable: {error:#}")))
                .await;
            return;
        }
    };
    while let Some(command) = rx.recv().await {
        let result = async {
            let created = match command {
                Command::Refresh => None,
                Command::Mutate(dialog) => execute(dialog, &mut control)
                    .await
                    .context("Action failed")?,
            };
            let sessions = load(&mut control, scope).await.context("Refresh failed")?;
            Ok(Loaded { sessions, created })
        }
        .await
        .map_err(|error: anyhow::Error| format!("{error:#}"));
        if tx.send(result).await.is_err() {
            return;
        }
    }
}
async fn execute(dialog: Dialog, control: &mut Control) -> Result<Option<String>> {
    match dialog.operation {
        Operation::Create => Ok(Some(
            actions::create(control, &dialog.selection, &dialog.text, dialog.direction).await?,
        )),
        Operation::Rename => {
            actions::rename(control, &dialog.selection, &dialog.text).await?;
            Ok(None)
        }
        Operation::Close => {
            actions::close(control, &dialog.selection).await?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::next_json,
        test_support::{control_pair, reply},
    };
    #[tokio::test]
    async fn refresh_is_bounded_and_shutdown_interrupts_a_slow_reply() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (control, mut server) = control_pair().await?;
                let mut data = Data::connect(async { Ok(control) }, None);
                let request = next_json(&mut server).await?;
                reply(&mut server, &request, json!({"creators":[]}), false).await?;
                let request = next_json(&mut server).await?;
                assert_eq!(request["method"], "session.list");
                // Repeated timers/manual refreshes cannot accumulate an RPC backlog.
                for _ in 0..100 {
                    data.refresh();
                }
                assert_eq!(data.requests.capacity(), 1);
                let mut dialog =
                    super::super::state::Browser::default().start_dialog("session.new")?;
                dialog.text = "preserve this label".into();
                assert_eq!(
                    data.mutate(dialog).expect("busy operation returned").text,
                    "preserve this label"
                );
                tokio::time::timeout(std::time::Duration::from_secs(1), data.stop()).await?;
                assert!(next_json(&mut server).await.is_err());
                Ok::<_, anyhow::Error>(())
            })
            .await
    }
    #[tokio::test]
    async fn a_failed_refresh_can_be_retried_without_replaying_mutations() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (control, mut server) = control_pair().await?;
                let mut data = Data::connect(async { Ok(control) }, None);
                for fail in [true, false] {
                    for method in ["block_creator.list", "session.list"] {
                        let request = next_json(&mut server).await?;
                        assert_eq!(request["method"], method);
                        reply(
                            &mut server,
                            &request,
                            json!({"sessions":[]}),
                            fail && method == "session.list",
                        )
                        .await?;
                    }
                    let result = data.changed().await.expect("worker result");
                    assert_eq!(result.is_err(), fail);
                    if fail {
                        data.refresh();
                    }
                }
                data.stop().await;
                Ok::<_, anyhow::Error>(())
            })
            .await
    }
}
