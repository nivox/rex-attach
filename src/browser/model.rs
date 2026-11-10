use crate::{activity::Snapshot, domain::Target};
use anyhow::{Context, Result};
use serde_json::Value;
#[derive(Clone, Debug)]
pub(super) struct Block {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) creator: String,
    pub(super) flavor: String,
    pub(super) terminal: bool,
    pub(super) default_label: bool,
}
#[derive(Clone, Debug)]
// Server windows are presented as tabs in Rex clients.
pub(super) struct Window {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) blocks: Vec<Block>,
    pub(super) default_label: bool,
    pub(super) focused_block: Option<String>,
}
#[derive(Clone, Debug)]
pub(super) struct Session {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) windows: Vec<Window>,
}

pub(super) fn values(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub(super) fn short(id: &str) -> &str {
    id.get(id.len().saturating_sub(6)..).unwrap_or(id)
}
pub(super) fn name(value: &Value, fallback: &str) -> String {
    value["label"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}
// Rex exposes no label provenance. Only match the generated label for this ID.
pub(super) fn default_label(value: &Value, kind: &str, id: &str) -> bool {
    let label = value["label"].as_str().unwrap_or("");
    label.is_empty()
        || label
            .strip_prefix(&format!("{kind} "))
            .is_some_and(|suffix| suffix.len() == 8 && id.ends_with(suffix))
}
impl Block {
    pub(super) fn recognize_flavor_default(&mut self, creators: &Value) {
        self.default_label |= values(&creators["creators"])
            .iter()
            .flat_map(|creator| values(&creator["flavors"]))
            .any(|flavor| {
                flavor["flavor"] == self.flavor
                    && (flavor["name"] == self.name || flavor["title"] == self.name)
            });
    }
    pub(super) fn display_name<'a>(&'a self, snapshot: &'a Snapshot, session: &str) -> &'a str {
        if self.default_label {
            let target = Target {
                session: session.into(),
                block: self.id.clone(),
            };
            if let Some(title) = snapshot
                .info(&target)
                .and_then(|info| info.title.as_deref())
                .filter(|title| !crate::text::clean(title).trim().is_empty())
            {
                return title;
            }
        }
        &self.name
    }
}
impl Window {
    pub(super) fn display_name<'a>(&'a self, snapshot: &'a Snapshot, session: &str) -> &'a str {
        if self.default_label {
            let focused = self
                .focused_block
                .as_deref()
                .and_then(|id| self.blocks.iter().find(|block| block.id == id))
                .or_else(|| self.blocks.first());
            if let Some(title) = focused
                .and_then(|block| {
                    snapshot.info(&Target {
                        session: session.into(),
                        block: block.id.clone(),
                    })
                })
                .and_then(|info| info.title.as_deref())
                .filter(|title| !crate::text::clean(title).trim().is_empty())
            {
                return title;
            }
        }
        &self.name
    }
}
pub(super) fn block(value: &Value) -> Result<Block> {
    let metadata: crate::wire::Block = crate::wire::decode(value, "block metadata")?;
    let id = metadata.block_id;
    let flavor = metadata.flavor.as_deref().unwrap_or("unknown flavor");
    let terminal = metadata.creator_name.as_deref() == Some("com.superlogical.terminal")
        || flavor.starts_with("com.superlogical.terminal.");
    Ok(Block {
        default_label: default_label(value, "Block", &id),
        name: name(value, &format!("Block {}", short(&id))),
        id,
        creator: value["creator_name"].as_str().unwrap_or("unknown").into(),
        flavor: flavor.into(),
        terminal,
    })
}

pub(super) fn session_tree(session: &Value, view: &Value, blocks: &[Value]) -> Result<Session> {
    let id = session["session_id"]
        .as_str()
        .context("session has no ID")?
        .to_owned();
    let decoded: crate::wire::SessionView = crate::wire::decode(view, "session view")?;
    let mut windows = Vec::new();
    for (index, (window, decoded)) in values(&view["windows"])
        .iter()
        .zip(&decoded.windows)
        .enumerate()
    {
        let mut children = Vec::new();
        for layer in &decoded.layers {
            for child in &layer.blocks {
                let child_id = child["block_id"]
                    .as_str()
                    .context("window block has no ID")?;
                if children.iter().any(|b: &Block| b.id == child_id) {
                    continue;
                }
                // list_blocks carries placement and ownership metadata; view
                // defines window/layer order and also covers concurrent additions.
                let metadata = blocks
                    .iter()
                    .find(|b| b["block_id"] == child_id)
                    .unwrap_or(child);
                children.push(block(metadata)?);
            }
        }
        windows.push(Window {
            default_label: default_label(
                window,
                "Window",
                window["window_id"].as_str().unwrap_or(""),
            ),
            focused_block: window["focused_block_id"].as_str().map(str::to_owned),
            id: decoded.window_id.clone(),
            name: name(window, &format!("Tab {}", index + 1)),
            blocks: children,
        });
    }
    let detached = blocks
        .iter()
        .filter(|b| b["placement"] == "detached")
        .map(block)
        .collect::<Result<Vec<_>>>()?;
    if !detached.is_empty() {
        windows.push(Window {
            default_label: false,
            focused_block: None,
            id: "detached".into(),
            name: "Detached blocks".into(),
            blocks: detached,
        });
    }
    Ok(Session {
        name: name(session, &id),
        id,
        windows,
    })
}
