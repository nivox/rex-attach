mod action;
mod actions;
mod activity;
mod browser;
mod graphics;
mod help;
mod mouse;
mod preview;
mod protocol;
mod raster;
mod shortcuts;
mod stream;
mod terminal;
mod tty;
mod vt;
mod wire;

mod app;
mod discovery;
mod domain;
mod session;
mod task;
mod text;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tokio::task::LocalSet::new().run_until(app::run()).await
}

#[cfg(test)]
mod test_support;
