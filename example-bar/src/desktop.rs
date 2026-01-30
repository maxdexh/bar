use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
impl WorkspaceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct BasicWorkspace {
    pub id: WorkspaceId,
    pub name: Arc<str>,
    pub monitor: Option<Arc<str>>,
    pub is_active: bool,
}

#[derive(Clone, Default, Debug)]
pub struct BasicDesktopState {
    pub workspaces: Vec<BasicWorkspace>,
}
