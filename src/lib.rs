//! `conn`: a local tool for one developer running many Claude Code terminal sessions.
//!
//! The crate is the whole program; `main.rs` only starts it. Module map:
//! - [`types`]: the domain, as sum types and witnesses. Everything else is handed these.
//! - [`events`]: the append-only hook-event log and the parse of one payload into a typed event.
//! - [`transcript`], [`sessions`], [`links`], [`gh`]: derivations over the log, the transcripts
//!   on disk, the supervisor and the `gh` CLI.
//! - [`jira`], [`reviews`]: the user's work items, and review requests heard in Slack.
//! - [`pam`]: the GCP PAM entitlements the user may request, their grants, and a request.
//! - [`gist`]: what each card's session is about and what the card waits for, written by a
//!   model.
//! - [`usage`]: how much of the plan's rate-limit windows is used.
//! - [`ledger`]: who owns which ticket, rollout or topic, what idle sessions wait for, and
//!   which rollouts run, as the sessions declare it.
//! - [`pending`], [`model`], [`state`]: the pending rule, the read model the pages render, and
//!   the builder and poller that keep it current.
//! - [`web`], [`replay`], [`snapshot`]: the three ways results leave the process.
//! - [`cli`], [`config`], [`util`]: the process edge.

pub mod cli;
pub mod config;
pub mod events;
pub mod gh;
pub mod gist;
pub mod jira;
pub mod ledger;
pub mod links;
pub mod map;
pub mod model;
pub mod pam;
pub mod pending;
pub mod replay;
pub mod reviews;
pub mod saves;
pub mod sessions;
pub mod snapshot;
pub mod state;
pub mod transcript;
pub mod types;
pub mod usage;
pub mod util;
pub mod web;

#[cfg(test)]
mod testutil;
