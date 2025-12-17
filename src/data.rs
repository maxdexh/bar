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

#[derive(Serialize, Deserialize, Debug)]
pub struct InteractGeneric<T> {
    pub location: Location,
    pub target: T,
    pub kind: InteractKind,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Location {
    pub x: u32,
    pub y: u32,
}
impl Location {
    pub const ZERO: Self = Location { x: 0, y: 0 };
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonitorId(Arc<str>);
impl From<&str> for MonitorId {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}
impl From<String> for MonitorId {
    fn from(value: String) -> Self {
        Self(value.into())
    }
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ActiveMonitorInfo {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub name: Arc<str>,
    pub id: MonitorId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BasicWorkspace {
    pub id: WorkspaceId,
    pub name: Arc<str>,
    pub monitor: Option<MonitorId>,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct BasicDesktopState {
    pub workspaces: Arc<[BasicWorkspace]>,
    pub monitors: Arc<[BasicMonitor]>,
    pub active_monitor: Option<MonitorId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BasicMonitor {
    pub id: MonitorId,
    pub active_workspace: WorkspaceId,
    pub name: Arc<str>,
}
