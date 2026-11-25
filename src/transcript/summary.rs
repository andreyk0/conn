//! What a transcript says about its session, folded record by record.
//!
//! **How a turn ends.** An interactive transcript ends a turn with a `system`/`turn_duration`
//! record. A headless (`-p`) transcript has none, so an assistant `end_turn` text block whose
//! next main-chain record is not a tool-result carrier ends it instead. The block is recorded
//! as a turn end at once, because the file may never get another record; a later
//! `turn_duration` moves that turn end to the real end, and a carrier right after the block
//! takes it back. A turn that is still open while a question waits has no end yet: that wait is
//! [`Summary::pending_ask`].

use crate::types::{
    AskUserQuestion, AssistantText, BackgroundTasks, Instant, SessionId, TurnEnd, TurnTrigger,
};

use super::record::{
    Attachment, Block, Chain, Envelope, Meta, PrLink, Prompt, Record, StopReason, SystemEvent,
    ToolCall, ToolOutcome, ToolResult, UserContent,
};
use super::tail::TailEvent;
use crate::types::outward::{Outward, Written};

/// How many human prompts [`Summary::human_prompts`] keeps.
///
/// Link extraction wants the recent thread of the conversation; fifty prompts reach back
/// well past the point where a human still means the links in them.
pub const HUMAN_PROMPTS_KEPT: usize = 50;

/// A kept prompt is cut to this many bytes, so fifty of them stay small beside a pasted log.
const HUMAN_PROMPT_BYTES_MAX: usize = 32 * 1024;

/// The trigger of a turn that no prompt in this file started: the file begins mid-turn, or a
/// record arrives after a turn ended without a new prompt.
const UNATTRIBUTED: &str = "unattributed";

/// A prompt the human typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanPrompt {
    pub at: Instant,
    /// Cut at [`HUMAN_PROMPT_BYTES_MAX`] bytes on a character boundary.
    pub text: String,
}

/// Everything derived from one transcript so far.
///
/// Build it with [`scan`](super::scan) or by folding [`TailEvent`]s into a
/// [`Summary::default`]; the private turn state means only those can produce a consistent one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// The first session id seen on a main-chain record.
    pub session_id: Option<SessionId>,
    /// The last working directory seen.
    pub cwd: Option<std::path::PathBuf>,
    /// The last git branch seen.
    pub git_branch: Option<String>,
    /// The last `ai-title`: the topic Claude Code derived.
    pub ai_title: Option<String>,
    /// The last `custom-title`: the name given by `-n`, `/rename` or a hook.
    pub custom_title: Option<String>,
    /// The last `last-prompt`.
    pub last_prompt: Option<String>,
    /// The last `permission-mode`.
    pub permission_mode: Option<String>,
    /// `cli`, `sdk-cli`, ...: the first one seen.
    pub entrypoint: Option<String>,
    /// Distinct by URL, in the order first seen.
    pub pr_links: Vec<PrLink>,
    /// The latest human prompt, queued ones included.
    pub last_human_prompt_at: Option<Instant>,
    /// The last [`HUMAN_PROMPTS_KEPT`] human prompts, oldest first.
    pub human_prompts: Vec<HumanPrompt>,
    /// When every human prompt was typed, oldest first: sixteen bytes a prompt, so the whole
    /// history of a session stays small enough for a replay to walk.
    pub human_prompt_times: Vec<Instant>,
    /// Every turn end, oldest first. Bounded by the file, which is read once per session.
    pub turn_ends: Vec<TurnEnd>,
    /// The latest `AskUserQuestion` no tool result has answered.
    pub pending_ask: Option<AskUserQuestion>,
    /// `AskUserQuestion` calls seen, answered or not.
    pub asks_made: u32,
    /// Of those, the ones a tool result answered.
    pub asks_answered: u32,
    /// Lines that yielded no record, whatever the reason.
    pub line_errors: u32,
    /// Bytes of the file that belong to lines already read; where a [`Tailer`](super::Tailer)
    /// resumes. Set by whoever reads the bytes, never by folding.
    pub bytes_read: u64,
    /// Bytes after the last newline: a line still being written, or one that never ended.
    pub unterminated_tail_bytes: u64,
    /// The latest timestamp on a main-chain record.
    pub last_record_at: Option<Instant>,
    /// The Jira and Slack writes that went through, oldest first; the last [`WRITES_KEPT`].
    pub writes: Vec<Written>,
    /// Writes asked for and not yet answered, with when they were asked: the last
    /// [`PENDING_WRITES_KEPT`].
    pending_writes: Vec<(String, Instant, Outward)>,
    turn: TurnState,
}

/// How many writes a summary keeps. A busy day writes a few dozen; the pages look back a week.
const WRITES_KEPT: usize = 200;

/// Calls run one after another, so a handful are ever unanswered at once.
const PENDING_WRITES_KEPT: usize = 16;

impl Summary {
    /// Records how much of the file the lines folded so far cover.
    pub const fn set_extent(&mut self, bytes_read: u64, unterminated_tail_bytes: u64) {
        self.bytes_read = bytes_read;
        self.unterminated_tail_bytes = unterminated_tail_bytes;
    }

    /// The title the session's terminal shows, as Claude Code picks it: the custom title,
    /// else the AI title. With neither it shows the placeholder `Claude Code`, which every
    /// such session shares, so no window can be told by it.
    pub fn window_title(&self) -> Option<&str> {
        self.custom_title.as_deref().or(self.ai_title.as_deref())
    }
}

/// Folds one event. A truncation discards the summary, because the bytes it described are gone.
pub fn fold_event(summary: &mut Summary, event: &TailEvent) {
    match event {
        TailEvent::Record(record) => fold(summary, record),
        TailEvent::Malformed(_) => summary.line_errors = summary.line_errors.saturating_add(1),
        TailEvent::Truncated => *summary = Summary::default(),
    }
}

/// Folds one record into the summary. Records of a subagent's chain change nothing.
pub fn fold(summary: &mut Summary, record: &Record) {
    match record {
        Record::User(user) => on_envelope(summary, &user.env, |summary| match &user.content {
            UserContent::Prompt(prompt) => summary.on_prompt(prompt, user.env.at),
            UserContent::ToolResults(results) => summary.on_tool_results(results.iter()),
            UserContent::Injected => summary.turn.settle(),
        }),
        Record::Assistant(assistant) => on_envelope(summary, &assistant.env, |summary| {
            summary.on_assistant(
                assistant.message_id.as_deref(),
                assistant.stop_reason.as_ref(),
                &assistant.blocks,
                assistant.env.at,
            );
        }),
        Record::System(system) => on_envelope(summary, &system.env, |summary| match system.event {
            SystemEvent::TurnDuration { .. } => summary.on_turn_duration(system.env.at),
            SystemEvent::Other { .. } => summary.turn.settle(),
        }),
        Record::Attachment(attachment) => {
            on_envelope(summary, &attachment.env, |summary| {
                match &attachment.attachment {
                    Attachment::QueuedCommand(prompt) => {
                        summary.note_human_prompt(prompt, attachment.env.at);
                        summary.turn.settle();
                    }
                    Attachment::Other { .. } => summary.turn.settle(),
                }
            });
        }
        Record::Meta(meta) => summary.on_meta(meta),
        Record::Unknown { .. } => {}
    }
}

/// Runs `then` for a main-chain record after taking the facts every record carries.
fn on_envelope(summary: &mut Summary, env: &Envelope, then: impl FnOnce(&mut Summary)) {
    match env.chain {
        Chain::Sidechain => {}
        Chain::Main => {
            summary.take_envelope(env);
            then(summary);
        }
    }
}

impl Summary {
    fn take_envelope(&mut self, env: &Envelope) {
        if self.session_id.is_none() {
            self.session_id.clone_from(&env.session_id);
        }
        if self.entrypoint.is_none() {
            self.entrypoint.clone_from(&env.entrypoint);
        }
        if env.cwd.is_some() {
            self.cwd.clone_from(&env.cwd);
        }
        if env.git_branch.is_some() {
            self.git_branch.clone_from(&env.git_branch);
        }
        self.last_record_at = self.last_record_at.max(Some(env.at));
    }

    fn on_meta(&mut self, meta: &Meta) {
        match meta {
            Meta::AiTitle(title) => self.ai_title = Some(title.clone()),
            Meta::CustomTitle(title) => self.custom_title = Some(title.clone()),
            Meta::LastPrompt(prompt) => self.last_prompt.clone_from(prompt),
            Meta::PermissionMode(mode) => self.permission_mode = Some(mode.clone()),
            Meta::PrLink(link) => {
                if !self.pr_links.iter().any(|known| known.url == link.url) {
                    self.pr_links.push(link.clone());
                }
            }
        }
    }

    fn note_human_prompt(&mut self, prompt: &Prompt, at: Instant) {
        match prompt.trigger {
            TurnTrigger::Human => {
                self.last_human_prompt_at = self.last_human_prompt_at.max(Some(at));
                self.human_prompt_times.push(at);
                if self.human_prompts.len() == HUMAN_PROMPTS_KEPT {
                    self.human_prompts.remove(0);
                }
                self.human_prompts.push(HumanPrompt {
                    at,
                    text: cut(&prompt.text, HUMAN_PROMPT_BYTES_MAX),
                });
            }
            TurnTrigger::TaskNotification
            | TurnTrigger::Peer
            | TurnTrigger::Scheduled
            | TurnTrigger::Coordinator
            | TurnTrigger::AutoContinuation
            | TurnTrigger::LocalCommand
            | TurnTrigger::CompactionSummary
            | TurnTrigger::Unknown(_) => {}
        }
    }

    fn on_prompt(&mut self, prompt: &Prompt, at: Instant) {
        self.note_human_prompt(prompt, at);
        self.turn.start(&prompt.trigger);
    }

    fn on_tool_results<'a>(&mut self, results: impl Iterator<Item = &'a ToolResult>) {
        self.turn.retract_text_end(&mut self.turn_ends);
        for result in results {
            let id = result.tool_use_id.as_str();
            if self.turn.answer(id, &mut self.turn_ends) {
                self.asks_answered = self.asks_answered.saturating_add(1);
            }
            self.settle_write(id, result.outcome);
        }
        self.pending_ask = self.turn.pending_ask();
    }

    /// A write is kept once its call is answered without an error; a refused or failed one
    /// changed nothing anyone reads.
    fn settle_write(&mut self, id: &str, outcome: ToolOutcome) {
        let Some(at) = self
            .pending_writes
            .iter()
            .position(|(pending, ..)| pending == id)
        else {
            return;
        };
        let (_, when, write) = self.pending_writes.remove(at);
        if outcome == ToolOutcome::Ok {
            if self.writes.len() >= WRITES_KEPT {
                self.writes.remove(0);
            }
            self.writes.push(Written { at: when, write });
        }
    }

    fn note_write(&mut self, id: &str, at: Instant, write: &Outward) {
        if self
            .pending_writes
            .iter()
            .any(|(pending, ..)| pending == id)
        {
            return;
        }
        if self.pending_writes.len() >= PENDING_WRITES_KEPT {
            self.pending_writes.remove(0);
        }
        self.pending_writes.push((id.to_owned(), at, write.clone()));
    }

    fn on_assistant(
        &mut self,
        message_id: Option<&str>,
        stop_reason: Option<&StopReason>,
        blocks: &[Block],
        at: Instant,
    ) {
        match self.turn.resume_for(message_id) {
            Resumed::TailOfEndedMessage => return,
            Resumed::Activity => {}
        }
        for block in blocks {
            match block {
                Block::Text(text) => {
                    self.turn.note_text(text);
                    if stop_reason == Some(&StopReason::EndTurn) {
                        self.turn.end(
                            at,
                            &mut self.turn_ends,
                            EndedBy::TextBlock {
                                message_id: message_id.map(str::to_owned),
                            },
                        );
                    }
                }
                Block::ToolUse(tool_use) => match &tool_use.call {
                    ToolCall::AskUserQuestion(ask) => {
                        self.asks_made = self.asks_made.saturating_add(1);
                        self.turn.ask(&tool_use.id, ask);
                        self.pending_ask = self.turn.pending_ask();
                    }
                    ToolCall::Outward(write) => self.note_write(&tool_use.id, at, write),
                    ToolCall::Other { .. } => {}
                },
                Block::Thinking | Block::Other { .. } => {}
            }
        }
    }

    fn on_turn_duration(&mut self, at: Instant) {
        self.turn.end_by_duration(at, &mut self.turn_ends);
    }
}

/// The turn the transcript is in, between records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnState {
    phase: Phase,
    /// `AskUserQuestion` calls no tool result has answered, oldest first.
    unanswered: Vec<(String, AskUserQuestion)>,
    /// For each turn end that carries an unanswered question: its index in `turn_ends` and the
    /// call, so the answer can clear it.
    asked_ends: Vec<(usize, String)>,
}

impl Default for TurnState {
    /// A file starts mid-turn as far as it knows.
    fn default() -> Self {
        Self {
            phase: Phase::Running(Open::unattributed()),
            unanswered: Vec::new(),
            asked_ends: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Running(Open),
    Ended { open: Open, by: EndedBy },
}

/// What is known of the turn that is running or just ended.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Open {
    trigger: TurnTrigger,
    /// The last assistant text block of the turn.
    text: Option<AssistantText>,
    /// The `AskUserQuestion` calls this turn made, oldest first.
    ask_ids: Vec<String>,
}

impl Open {
    fn unattributed() -> Self {
        Self::new(TurnTrigger::Unknown(UNATTRIBUTED.to_owned()))
    }

    const fn new(trigger: TurnTrigger) -> Self {
        Self {
            trigger,
            text: None,
            ask_ids: Vec::new(),
        }
    }
}

/// The evidence that a turn ended, which decides what may still revise it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EndedBy {
    /// An `end_turn` text block, and nothing since. A tool-result carrier next takes it back.
    TextBlock { message_id: Option<String> },
    /// An `end_turn` text block that a later non-carrier record confirmed.
    Settled,
    /// A `turn_duration` record: final.
    Duration,
}

/// What an assistant record means for the turn state.
enum Resumed {
    /// A further block of the message that already ended the turn: nothing new happened.
    TailOfEndedMessage,
    /// The model is working in a running turn.
    Activity,
}

impl TurnState {
    fn open_mut(&mut self) -> &mut Open {
        match &mut self.phase {
            Phase::Running(open) | Phase::Ended { open, .. } => open,
        }
    }

    /// A prompt starts a turn. A compaction summary inside a running turn is the same turn
    /// carrying on after Claude Code compacted its context.
    fn start(&mut self, trigger: &TurnTrigger) {
        let continues =
            *trigger == TurnTrigger::CompactionSummary && matches!(self.phase, Phase::Running(_));
        if !continues {
            self.phase = Phase::Running(Open::new(trigger.clone()));
        }
    }

    /// A record that is neither a tool-result carrier nor an assistant block: it confirms a
    /// turn end that rests on a text block.
    fn settle(&mut self) {
        if let Phase::Ended { by, .. } = &mut self.phase
            && matches!(by, EndedBy::TextBlock { .. })
        {
            *by = EndedBy::Settled;
        }
    }

    /// An assistant record continues the running turn, or starts an unprompted one after an
    /// end, unless it is another block of the message whose text block ended the turn.
    fn resume_for(&mut self, message_id: Option<&str>) -> Resumed {
        let same_message = matches!(
            &self.phase,
            Phase::Ended { by: EndedBy::TextBlock { message_id: ended }, .. }
                if message_id.is_some() && ended.as_deref() == message_id
        );
        if same_message {
            return Resumed::TailOfEndedMessage;
        }
        if let Phase::Ended { open, .. } = &self.phase {
            self.phase = Phase::Running(Open::new(open.trigger.clone()));
        }
        Resumed::Activity
    }

    fn note_text(&mut self, text: &str) {
        self.open_mut().text = Some(AssistantText::of(text));
    }

    fn ask(&mut self, id: &str, ask: &AskUserQuestion) {
        self.open_mut().ask_ids.push(id.to_owned());
        self.unanswered.push((id.to_owned(), ask.clone()));
    }

    fn pending_ask(&self) -> Option<AskUserQuestion> {
        self.unanswered.last().map(|(_, ask)| ask.clone())
    }

    /// Takes back a text-block turn end when a tool result follows it directly: the model was
    /// not done.
    fn retract_text_end(&mut self, turn_ends: &mut Vec<TurnEnd>) {
        let Phase::Ended { open, by } = &self.phase else {
            return;
        };
        match by {
            EndedBy::TextBlock { .. } => {
                let open = open.clone();
                turn_ends.pop();
                self.asked_ends
                    .retain(|(index, _)| *index < turn_ends.len());
                self.phase = Phase::Running(open);
            }
            EndedBy::Settled | EndedBy::Duration => {}
        }
    }

    /// Marks the call answered and clears it from the turn end that was waiting on it.
    /// Whether the call was one still unanswered.
    fn answer(&mut self, id: &str, turn_ends: &mut [TurnEnd]) -> bool {
        let unanswered_before = self.unanswered.len();
        self.unanswered.retain(|(call, _)| call != id);
        let was_waiting = self.unanswered.len() < unanswered_before;
        let (answered, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.asked_ends)
            .into_iter()
            .partition(|(_, call)| call == id);
        for (index, _) in answered {
            if let Some(end) = turn_ends.get_mut(index) {
                end.asked = None;
            }
        }
        self.asked_ends = waiting;
        was_waiting
    }

    fn end(&mut self, at: Instant, turn_ends: &mut Vec<TurnEnd>, by: EndedBy) {
        let open = self.open_mut().clone();
        let asked = self
            .unanswered
            .iter()
            .rev()
            .find(|(id, _)| open.ask_ids.contains(id));
        if let Some((id, _)) = asked {
            self.asked_ends.push((turn_ends.len(), id.clone()));
        }
        turn_ends.push(TurnEnd {
            at,
            trigger: open.trigger.clone(),
            text: open.text.clone().unwrap_or_else(|| AssistantText::of("")),
            background_tasks: BackgroundTasks::Unknown,
            asked: asked.map(|(_, ask)| ask.clone()),
        });
        self.phase = Phase::Ended { open, by };
    }

    /// `turn_duration` is the authoritative end: it moves a text-block end to its own time, and
    /// ends a running turn that no text block ended (an interrupted one, one that stopped on a
    /// tool call).
    fn end_by_duration(&mut self, at: Instant, turn_ends: &mut Vec<TurnEnd>) {
        match &mut self.phase {
            Phase::Ended { by, .. } => {
                if let Some(end) = turn_ends.last_mut() {
                    end.at = at;
                }
                *by = EndedBy::Duration;
            }
            Phase::Running(_) => self.end(at, turn_ends, EndedBy::Duration),
        }
    }
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cut(text: &str, max: usize) -> String {
    let end = text.floor_char_boundary(max);
    text.get(..end).unwrap_or(text).to_owned()
}
