//! Registry-resolved node kind. Collection names are open; these constants
//! select existing compiled behavior and core singleton storage only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeKind<'a>(&'a str);

#[allow(non_upper_case_globals)]
impl<'a> NodeKind<'a> {
    pub const Task: Self = Self("task");
    pub const Decision: Self = Self("decision");
    pub const Glossary: Self = Self("glossary");
    pub const Artifact: Self = Self("artifact");
    pub const Project: Self = Self("project");
    pub const Goal: Self = Self("goal");
    pub const Handoff: Self = Self("handoff");

    pub fn collection(collection: &'a str) -> Self {
        match collection {
            "tasks" => Self::Task,
            "decisions" => Self::Decision,
            "glossary" => Self::Glossary,
            "artifacts" => Self::Artifact,
            name => Self(name),
        }
    }
    pub fn singleton(kind: &str) -> Option<Self> {
        match kind {
            "project" => Some(Self::Project),
            "goal" => Some(Self::Goal),
            "handoff" => Some(Self::Handoff),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'a str {
        self.0
    }
    pub fn layer_name(self) -> &'a str {
        self.0
    }
    pub fn collection_name(self) -> Option<&'a str> {
        match self {
            Self::Project | Self::Goal | Self::Handoff => None,
            Self::Task => Some("tasks"),
            Self::Decision => Some("decisions"),
            Self::Artifact => Some("artifacts"),
            _ => Some(self.0),
        }
    }
    pub fn artifact_name(self) -> &'static str {
        match self {
            Self::Project => "project file",
            Self::Goal => "goal file",
            Self::Handoff => "handoff file",
            Self::Task => "task file",
            Self::Decision => "decisions file",
            Self::Glossary => "glossary file",
            _ => "node file",
        }
    }
}
