use std::sync::Arc;

use crate::layout::PanelSpecData;

/// A rasterized panel frame ready to be committed to a display.
///
/// Pixel data is held behind `Arc<[u8]>` so the pipeline, the command
/// channel, and the presenter's coalescing buffer can share one allocation.
#[derive(Clone)]
pub struct PanelFrame {
    pub pixels: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

/// The typed vocabulary the pipeline speaks to the presenter.
///
/// Lifecycle variants (`Create`, `Move`, `Resize`, `Delete`) are ordered and
/// discrete; the presenter applies them immediately. `UpdatePicture` is
/// latest-wins per panel id — the presenter coalesces multiple updates and
/// commits only the most recent one when the backend is ready.
pub enum PanelCommand {
    Create(PanelSpecData),
    Move(PanelSpecData),
    Resize(PanelSpecData),
    Delete { id: String },
    UpdatePicture { id: String, frame: PanelFrame },
}
