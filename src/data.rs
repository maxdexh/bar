use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub enum InteractKind {
    Hover,
    Click(crossterm::event::MouseButton),
    Scroll(Direction),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

// TODO: Remove in favor of concrete variant
#[derive(Serialize, Deserialize, Debug)]
pub struct InteractGeneric<T> {
    pub location: Position32,
    pub target: T,
    pub kind: InteractKind,
}

#[derive(Default, Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position32 {
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkspaceId(Arc<str>);
impl From<&str> for WorkspaceId {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}
impl From<String> for WorkspaceId {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BasicWorkspace {
    pub id: WorkspaceId,
    pub name: Arc<str>,
    pub monitor: Option<Arc<str>>,
    pub is_active: bool,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct BasicDesktopState {
    pub workspaces: Vec<BasicWorkspace>,
}
