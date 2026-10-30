//! Typed shapes for the parts of `session.view` and `session.list` the client draws.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionView {
    pub revision: u64,
    pub session_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub active_window_id: Option<String>,
    #[serde(default)]
    pub windows: Vec<WindowView>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WindowView {
    pub window_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused_block_id: Option<String>,
    #[serde(default)]
    pub zoomed_block_id: Option<String>,
    #[serde(default)]
    pub layers: Vec<LayerView>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayerView {
    #[serde(default)]
    pub layout: Option<LayoutNode>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutNode {
    /// `[x, y, w, h]` as fractions of the window.
    pub rect: [f32; 4],
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub split: Option<Box<Split>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Split {
    pub before: LayoutNode,
    pub after: LayoutNode,
}

impl LayoutNode {
    /// Every block in the tree with its rect, in layout order.
    pub fn leaves(&self, out: &mut Vec<(String, [f32; 4])>) {
        if let Some(block_id) = &self.block_id {
            out.push((block_id.clone(), self.rect));
        }
        if let Some(split) = &self.split {
            split.before.leaves(out);
            split.after.leaves(out);
        }
    }
}

impl WindowView {
    /// Every block in the window's layouts, whether or not a zoom hides it.
    pub fn all_blocks(&self) -> Vec<String> {
        let mut out = Vec::new();
        for layer in &self.layers {
            if let Some(layout) = &layer.layout {
                layout.leaves(&mut out);
            }
        }
        out.into_iter().map(|(id, _)| id).collect()
    }

    /// The blocks to draw, back to front, each with its rect as fractions of the window.
    pub fn placed_blocks(&self) -> Vec<(String, [f32; 4])> {
        if let Some(zoomed) = &self.zoomed_block_id {
            return vec![(zoomed.clone(), [0.0, 0.0, 1.0, 1.0])];
        }
        let mut out = Vec::new();
        for layer in &self.layers {
            if let Some(layout) = &layer.layout {
                layout.leaves(&mut out);
            }
        }
        out
    }
}
