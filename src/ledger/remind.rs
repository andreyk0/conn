//! What `conn hook` tells a session about its place in the ledger.
//!
//! Text a `SessionStart` or `UserPromptSubmit` hook prints is added to the session's context,
//! so this is how a session learns the ledger exists, what it owns after a compaction, and
//! that the rollout it waited on has landed: nobody has to relay it.

use std::fmt::Write as _;

use super::Ledger;
use super::command::{Context, wait_state_words};
use crate::types::SessionId;

/// When the hook speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// A session starts, resumes, or comes back from a compaction or a clear.
    Start,
    /// The human typed a prompt.
    Prompt,
}

/// How the ledger is used, said once at each start. Kept short: every session reads it.
const HOW: &str = "\
conn: sessions on this machine coordinate through the `conn` ledger.
- Before you act on a ticket, rollout or follow-up another session may also touch, run `conn who \
<it>`. If no one owns it, `conn claim <it>` (ABC-123, rollout:<name>, topic:<name>); if someone \
does, send them what you have (SendMessage) instead of acting on it. Jira writes to a ticket \
another running session owns are refused.
- When you stop to wait for a rollout, another session's ticket or topic, a time or a person, run \
`conn wait rollout:<name> | ABC-123 | topic:<name> | --until <time> | --for \"<what>\" --then \
\"<what you will do>\"`; `conn wait --clear` once you pick the work back up. A release, \
including one only the user can run, is a rollout: `conn wait rollout:<name>`, never `--for`. \
When your part of a ticket or topic others wait on is done, `conn release` it: that ends their \
wait. A time or `--for` wait that is about a ticket takes `--ticket ABC-123`: the wait is over \
once Jira resolves it, whoever closes it.
- When you run a release: `conn rollout start <name> --ships <ticket>...` before, `conn rollout \
finish <name> --outcome shipped|rolled-back|abandoned` after. Report to each shipped ticket's \
owner; leave the ticket to it.
- When a shipped ticket's exit criteria can only be checked after a date or a period, run `conn \
defer ABC-123 --until <time> --check \"<what to check then>\"`, so nobody checks it before then.";

/// The text for `session` at this moment, or nothing to say.
pub fn text(
    moment: Moment,
    ledger: &Ledger,
    session: &SessionId,
    context: &Context<'_>,
) -> Option<String> {
    let mut out = String::new();
    if moment == Moment::Start {
        out.push_str(HOW);
        let owned = ledger.claims_of(session);
        if !owned.is_empty() {
            let names: Vec<String> = owned.iter().map(ToString::to_string).collect();
            let _ = write!(out, "\nThis session owns: {}.", names.join(", "));
        }
    }
    if let Some(state) = ledger.wait_of(session, context.now) {
        let words = wait_state_words(state, context);
        if state.is_over() {
            let _ = write!(
                out,
                "\nconn: {words}. Run `conn wait --clear` once you have picked it up."
            );
        } else {
            let _ = write!(
                out,
                "\nconn: this session is {words}. If this prompt changes that, run `conn wait \
                 --clear` or a new `conn wait`."
            );
        }
    }
    let out = out.trim_start().to_owned();
    (!out.is_empty()).then_some(out)
}
