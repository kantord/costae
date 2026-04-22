use crate::layout::PanelSpecData;

pub struct Position {
    pub x: i32,
    pub y: i32,
}

pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

pub trait DisplayManager {
    type Panel;
    type Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static;

    fn create_window(&mut self, spec: &PanelSpecData) -> Result<Self::Panel, Self::Error>;
    fn update_position(&mut self, panel: &mut Self::Panel, spec: &PanelSpecData) -> Result<(), Self::Error>;
    fn update_dimensions(&mut self, panel: &mut Self::Panel, spec: &PanelSpecData) -> Result<(), Self::Error>;
    fn update_image(&mut self, panel: &mut Self::Panel, bgrx: &[u8]) -> Result<(), Self::Error>;
    fn delete_window(&mut self, panel: Self::Panel) -> Result<(), Self::Error>;
    fn flush(&mut self) {}
}
