//! A cancellable local worker owns its resources for the entire future.
//! Cancelling a protocol worker drops its dedicated connection, so partially
//! completed RPCs are never resumed or retried on a shared connection.
use std::future::Future;
use tokio::{sync::oneshot, task::JoinHandle};

pub struct Task {
    cancel: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}
impl Task {
    pub fn spawn(work: impl Future<Output = ()> + 'static) -> Self {
        let (cancel, cancelled) = oneshot::channel();
        let join = tokio::task::spawn_local(async move {
            tokio::select! { _ = work => {}, _ = cancelled => {} }
        });
        Self {
            cancel: Some(cancel),
            join: Some(join),
        }
    }
    pub async fn stop(mut self) {
        self.cancel.take();
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel.take();
    }
}
