//! The domain, as sum types and witnesses.
//!
//! Every value a later module handles is one of these. Plain records have public fields;
//! a type with an invariant has private fields and one constructor, and holding the type is
//! the evidence the check ran.

mod activity;
mod age;
mod file;
mod ids;
mod link;
pub mod outward;
mod pending;
mod resume;
mod session;
mod text;
mod trigger;
mod turn;

pub use activity::{Activity, ActivityKind, BgState, ListingStatus, WaitingFor};
pub use age::{Age, secs_since};
pub use file::{FileRootError, FileRoots, LocalFile, LocalFileError, is_markdown};
pub use ids::{Pid, ProjectDirName, ProjectDirNameError, SessionId, SessionIdError};
pub use link::{
    Host, HostError, Identifier, JiraKey, JiraPrefix, JiraPrefixError, Link, LinkParseError,
    LinkRenderError, LinkTargets, SafeUrl, SafeUrlError,
};
pub use pending::PendingCard;
pub use resume::{ResumeCommand, ResumeFlag, ResumeUnavailable, ResumeWarning};
pub use session::{Session, SessionKind};
pub use text::{NonBlank, NonBlankError};
pub use trigger::PromptFacts;
pub use turn::{
    AskUserQuestion, AssistantText, BackgroundTask, BackgroundTasks, MultiSelect, NoQuestions,
    Question, QuestionOption, TaskKind, TaskStatus, TurnEnd, TurnTrigger,
};

/// A moment in time, always UTC; converted to local time only at display.
pub use jiff::Timestamp as Instant;
