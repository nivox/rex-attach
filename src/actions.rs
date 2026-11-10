//! Rex.app action names implemented locally through explicit server methods.
use crate::{
    action::Action,
    domain::{Selection, Target},
    protocol::Control,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

pub fn layout(label: &str) -> Value {
    json!({"block":{"flavor":"com.superlogical.terminal.shell","label":label}})
}

pub async fn rename(control: &mut Control, selection: &Selection, label: &str) -> Result<()> {
    let (method, mut args) = match selection {
        Selection::Session(s) => ("session.set_label", json!({"session_id":s})),
        Selection::Window { session, window } => (
            "session.set_window_label",
            json!({"session_id":session,"window_id":window}),
        ),
        Selection::Block(t) => (
            "session.set_block_label",
            json!({"session_id":t.session,"block_id":t.block}),
        ),
    };
    args["label"] = label.into();
    scoped(control, selection.session(), method, args).await?;
    Ok(())
}
pub async fn close(control: &mut Control, selection: &Selection) -> Result<()> {
    let (method, args) = match selection {
        Selection::Session(s) => ("session.destroy", json!({"session_id":s})),
        Selection::Window { session, window } => (
            "session.close_window",
            json!({"session_id":session,"window_id":window}),
        ),
        Selection::Block(t) => (
            "block.close",
            json!({"session_id":t.session,"block_id":t.block}),
        ),
    };
    scoped(control, selection.session(), method, args).await?;
    Ok(())
}
async fn scoped(control: &mut Control, session: &str, method: &str, args: Value) -> Result<Value> {
    crate::session::with_session(control, session, async |control, _| {
        control.call(method, args).await
    })
    .await
}
/// Returns the created session/window/block ID for browser selection restoration.
pub async fn create(
    control: &mut Control,
    selection: &Selection,
    label: &str,
    direction: &str,
) -> Result<String> {
    match selection {
        Selection::Session(_) => {
            let reply = control
                .call(
                    "session.create",
                    json!({"label":label,"initial_windows":[{"layout":layout("")}]}),
                )
                .await?;
            Ok(reply["session_id"]
                .as_str()
                .context("created session has no ID")?
                .into())
        }
        Selection::Window { session, .. } => {
            let reply=scoped(control,session,"session.new_window",json!({"session_id":session,"window_label":label,"layout":layout(""),"focus":false})).await?;
            Ok(reply["window_id"]
                .as_str()
                .context("created window has no ID")?
                .into())
        }
        Selection::Block(t) => {
            let reply=scoped(control,&t.session,"session.new_split",json!({"session_id":t.session,"anchor_id":t.block,"direction":direction,"side":"after","layout":layout(label),"focus":false})).await?;
            created_block(&reply)
        }
    }
}
fn created_block(reply: &Value) -> Result<String> {
    Ok(reply["block_ids"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .context("creation returned no blocks")?
        .into())
}
fn blocks(window: &Value) -> Vec<&Value> {
    window["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|l| l["blocks"].as_array().into_iter().flatten())
        .collect()
}
fn terminal(block: &Value) -> bool {
    block["creator_name"] == "com.superlogical.terminal"
        || block["flavor"]
            .as_str()
            .is_some_and(|f| f.starts_with("com.superlogical.terminal."))
}
fn destination(view: &Value, target: &Target, action: Action) -> Result<String> {
    let windows = view["windows"]
        .as_array()
        .context("session has no windows")?;
    let index = windows
        .iter()
        .position(|w| blocks(w).iter().any(|b| b["block_id"] == target.block))
        .context("attached block is not in a window")?;
    if action == "pane.focus_next" {
        let children: Vec<_> = blocks(&windows[index])
            .into_iter()
            .filter(|b| terminal(b))
            .collect();
        let current = children
            .iter()
            .position(|b| b["block_id"] == target.block)
            .context("attached block is not a terminal")?;
        return Ok(children[(current + 1) % children.len()]["block_id"]
            .as_str()
            .context("block has no ID")?
            .into());
    }
    let forward = action == "client.tab.next";
    for offset in 1..=windows.len() {
        let next = if forward {
            (index + offset) % windows.len()
        } else {
            (index + windows.len() - offset) % windows.len()
        };
        let children: Vec<_> = blocks(&windows[next])
            .into_iter()
            .filter(|b| terminal(b))
            .collect();
        if let Some(block) = children
            .iter()
            .find(|b| b["block_id"] == windows[next]["focused_block_id"])
            .or_else(|| children.first())
        {
            return Ok(block["block_id"]
                .as_str()
                .context("block has no ID")?
                .into());
        }
    }
    bail!("no terminal blocks in session")
}
/// The caller already holds an attachment. This never detaches it during an RPC.
pub async fn terminal_action(
    control: &mut Control,
    target: &Target,
    action: Action,
) -> Result<Target> {
    let block = match action.as_str() {
        "pane.split" | "pane.split.right" | "pane.split.down" => {
            let direction = if action == "pane.split.down" {
                "vertical"
            } else {
                "horizontal"
            };
            let reply=control.call("session.new_split",json!({"session_id":target.session,"anchor_id":target.block,"direction":direction,"side":"after","layout":layout(""),"focus":true})).await?;
            created_block(&reply)?
        }
        "client.tab.new" => {
            let reply = control
                .call(
                    "session.new_window",
                    json!({"session_id":target.session,"layout":layout(""),"focus":true}),
                )
                .await?;
            created_block(&reply)?
        }
        "pane.focus_next" | "client.tab.next" | "client.tab.previous" => {
            let view = control
                .call("session.view", json!({"session_id":target.session}))
                .await?;
            destination(&view, target, action)?
        }
        _ => bail!("unsupported terminal action: {action}"),
    };
    ensure!(!block.is_empty(), "empty target block");
    control
        .call(
            "session.focus_block",
            json!({"session_id":target.session,"block_id":block}),
        )
        .await?;
    Ok(Target {
        session: target.session.clone(),
        block,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn navigation_uses_attached_window_and_wraps_preserving_window_focus() {
        let b = |id| json!({"block_id":id,"creator_name":"com.superlogical.terminal"});
        let view = json!({"active_window_id":"window:unrelated","windows":[
            {"focused_block_id":"b","layers":[{"blocks":[b("a"),b("b")]}]},
            {"layers":[{"blocks":[{"block_id":"web","creator_name":"web"}]}]},
            {"focused_block_id":"d","layers":[{"blocks":[b("c"),b("d")]}]}
        ]});
        let t = Target {
            session: "s".into(),
            block: "b".into(),
        };
        assert_eq!(
            destination(&view, &t, "pane.focus_next".into()).unwrap(),
            "a"
        );
        assert_eq!(
            destination(&view, &t, "client.tab.next".into()).unwrap(),
            "d"
        );
        assert_eq!(
            destination(&view, &t, "client.tab.previous".into()).unwrap(),
            "d"
        );
        let t = Target {
            block: "d".into(),
            ..t
        };
        assert_eq!(
            destination(&view, &t, "client.tab.next".into()).unwrap(),
            "b"
        );
    }
}
