#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventResult {
    Continue,
    Quit,
}

pub struct EventHandler;

impl EventHandler {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EventHandler {
    fn default() -> Self {
        Self::new()
    }
}
