pub mod wayland;

/// Platform tag identifying which display backend owns a panel.
/// Carries no live connection state — the real connection lives in `TickState`.
/// This keeps `DisplayContext` freely constructible in tests without a display.
#[derive(Debug)]
pub enum DisplayContext {
    X11,
    Wayland,
}

impl DisplayContext {
    pub fn test_x11() -> Self { DisplayContext::X11 }
    pub fn test_wayland() -> Self { DisplayContext::Wayland }
}

#[derive(Debug)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Other(u32),
}

#[derive(Debug)]
pub enum WindowEvent {
    Click {
        panel_id: String,
        x_logical: f32,
        y_logical: f32,
        button: MouseButton,
    },
    OutputsChanged,
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("connection lost")]
    ConnectionLost,
}

pub trait DisplayServer {
    fn as_raw_fd(&self) -> std::os::fd::RawFd;
    fn dispatch(&mut self) -> Result<Vec<WindowEvent>, DispatchError>;
}
