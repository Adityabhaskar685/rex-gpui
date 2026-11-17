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
    /// `horizontal` puts `before` left of `after`; `vertical` puts it above.
    #[serde(default)]
    pub direction: String,
    pub before: LayoutNode,
    pub after: LayoutNode,
}

/// The border between the two sides of a split, which can be dragged to resize them.
#[derive(Debug, Clone, PartialEq)]
pub struct Border {
    /// True for a border between left and right; false for one between top and bottom.
    pub vertical_line: bool,
    /// A block on each side, which together name the split to `session.resize_pane`.
    pub before_block: String,
    pub after_block: String,
    /// The split's `[x, y, w, h]`, as fractions of the window.
    pub rect: [f32; 4],
    /// Where the border is, as a fraction of the window's width or height.
    pub at: f32,
}

impl Border {
    pub fn direction(&self) -> &'static str {
        if self.vertical_line {
            "horizontal"
        } else {
            "vertical"
        }
    }

    /// The ratio that puts the border at `at`, a fraction of the window's width or height.
    pub fn ratio_at(&self, at: f32) -> f32 {
        let [x, y, w, h] = self.rect;
        let (start, len) = if self.vertical_line { (x, w) } else { (y, h) };
        ((at - start) / len.max(f32::EPSILON)).clamp(0.05, 0.95)
    }
}

impl LayoutNode {
    fn first_block(&self) -> Option<&String> {
        self.block_id
            .as_ref()
            .or_else(|| self.split.as_ref().and_then(|s| s.before.first_block()))
    }

    /// The border of every split in the tree.
    pub fn borders(&self, out: &mut Vec<Border>) {
        let Some(split) = &self.split else { return };
        if let (Some(before), Some(after)) = (split.before.first_block(), split.after.first_block())
        {
            let vertical_line = split.direction != "vertical";
            let [ax, ay, ..] = split.after.rect;
            out.push(Border {
                vertical_line,
                before_block: before.clone(),
                after_block: after.clone(),
                rect: self.rect,
                at: if vertical_line { ax } else { ay },
            });
        }
        split.before.borders(out);
        split.after.borders(out);
    }

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

    /// The borders between the tiled layout's panes. None while a block is zoomed.
    pub fn borders(&self) -> Vec<Border> {
        let mut out = Vec::new();
        if self.zoomed_block_id.is_none()
            && let Some(layout) = self.layers.first().and_then(|l| l.layout.as_ref())
        {
            layout.borders(&mut out);
        }
        out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_borders_and_ratios() {
        // Left half, and a right half split top and bottom.
        let view: WindowView = serde_json::from_value(serde_json::json!({
            "window_id": "w",
            "layers": [{"layout": {"rect": [0, 0, 1, 1], "split": {
                "direction": "horizontal",
                "before": {"rect": [0, 0, 0.5, 1], "block_id": "a"},
                "after": {"rect": [0.5, 0, 0.5, 1], "split": {
                    "direction": "vertical",
                    "before": {"rect": [0.5, 0, 0.5, 0.25], "block_id": "b"},
                    "after": {"rect": [0.5, 0.25, 0.5, 0.75], "block_id": "c"},
                }},
            }}}],
        }))
        .unwrap();
        let borders = view.borders();
        assert_eq!(borders.len(), 2);
        let (outer, inner) = (&borders[0], &borders[1]);
        assert_eq!(
            (
                outer.direction(),
                outer.before_block.as_str(),
                outer.after_block.as_str()
            ),
            ("horizontal", "a", "b")
        );
        assert_eq!(outer.at, 0.5);
        assert_eq!(
            (
                inner.direction(),
                inner.before_block.as_str(),
                inner.after_block.as_str()
            ),
            ("vertical", "b", "c")
        );
        assert_eq!(inner.at, 0.25);
        // Dragging the inner border to 40% of the window's height: 40% of its split.
        assert!((inner.ratio_at(0.4) - 0.4).abs() < 1e-6);
        assert!((outer.ratio_at(0.75) - 0.75).abs() < 1e-6);
        assert_eq!(outer.ratio_at(1.2), 0.95);
    }
}
