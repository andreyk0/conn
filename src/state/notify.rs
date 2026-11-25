//! The desktop notification: one when a card the human has not seen appears, never one per poll.
//!
//! A card is identified by its session and its `since`, so a finished turn is a new card even
//! when the session already has one waiting, and the same card seen again after a source blinked
//! is not new. A quota hold never notifies: nothing the human does ends it. The notification
//! carries a count and nothing else, so no session text reaches the notification daemon.

use std::collections::HashSet;
use std::time::Duration;

use jiff::SignedDuration;

use crate::model::{Model, Notifications, Seen, Tier};
use crate::types::{Instant, SessionId};
use crate::util::runner::{CommandRunner, Exit, RunError};

const PROGRAM: &str = "notify-send";
const TITLE: &str = "conn";
/// `notify-send` hands the message to the daemon and returns; five seconds means it is stuck.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The shortest time between two notifications, however many cards arrive.
///
/// Cards that arrive together have one cause: a coordinator's workers finishing in the same
/// minute, or a source recovering after a failed read. One look at the page answers all of
/// them. The slowest such burst is a listing cycle of 10 s plus the 30 s an attempt may take
/// (`LISTING_COVER`'s own derivation); 60 s doubles that, so a burst is one notification and
/// cards a minute apart are two.
const DEBOUNCE: SignedDuration = SignedDuration::from_secs(60);

/// How long a card stays remembered as announced after it leaves the inbox.
///
/// A card leaves when a source fails and comes back when it recovers, which takes minutes; a
/// day is far above that and bounds the memory to the cards of one day.
const RETENTION: SignedDuration = SignedDuration::from_hours(24);

/// One card: the session and the moment it has been waiting since.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CardKey {
    session: SessionId,
    since: Instant,
}

/// The cards the human has not seen and that a notification may announce.
fn unseen(model: &Model) -> HashSet<CardKey> {
    model
        .inbox
        .iter()
        .filter(|entry| entry.seen == Seen::Unseen && entry.rank.tier() != Tier::QuotaHold)
        .map(|entry| CardKey {
            session: entry.session.clone(),
            since: entry.rank.since(),
        })
        .collect()
}

/// Watches the unseen cards and sends the notification when one appears that it has not
/// announced, at most once per [`DEBOUNCE`].
#[derive(Debug)]
pub struct Notifier {
    announced: HashSet<CardKey>,
    last_attempt: Option<Instant>,
    state: Notifications,
}

impl Default for Notifier {
    fn default() -> Self {
        Self {
            announced: HashSet::new(),
            last_attempt: None,
            state: Notifications::Untried,
        }
    }
}

impl Notifier {
    pub const fn state(&self) -> &Notifications {
        &self.state
    }

    /// Looks at the model just built and sends the notification if a card is new and the
    /// debounce has passed. Never fails: what happened is in [`Notifier::state`], which the
    /// model's health shows. A card that arrives inside the debounce is announced at the first
    /// observation after it.
    pub async fn observe(&mut self, model: &Model, runner: &dyn CommandRunner, now: Instant) {
        let current = unseen(model);
        let gained = current.iter().any(|key| !self.announced.contains(key));
        self.announced
            .retain(|key| current.contains(key) || now.duration_since(key.since) <= RETENTION);
        if !gained || self.debounced(now) {
            return;
        }
        // A missing program is recorded once and not asked for again.
        if matches!(self.state, Notifications::Missing { .. }) {
            return;
        }
        self.last_attempt = Some(now);
        let message = format!("{} sessions need you", current.len());
        self.state = match runner.run(PROGRAM, &[TITLE, &message], TIMEOUT).await {
            Ok(output) if output.exit == Exit::Success => {
                self.announced.extend(current);
                Notifications::Sent { at: now }
            }
            Ok(output) => Notifications::Failed {
                at: now,
                reason: format!("{PROGRAM} exited with {:?}: {}", output.exit, output.stderr),
            },
            Err(RunError::NotFound) => {
                tracing::warn!("{PROGRAM} is not installed; desktop notifications are off");
                Notifications::Missing { at: now }
            }
            Err(error) => Notifications::Failed {
                at: now,
                reason: error.to_string(),
            },
        };
    }

    fn debounced(&self, now: Instant) -> bool {
        self.last_attempt
            .is_some_and(|last| now.duration_since(last) < DEBOUNCE)
    }
}

#[cfg(test)]
mod tests {
    //! The notifier is driven through `FakeRunner`, whose recorded calls are the whole
    //! observable effect, over hand-built models whose cards are (session, since, seen).

    use super::*;
    use crate::model::InboxEntry;
    use crate::testutil::{secs as at, session_id};
    use crate::types::{PendingCard, WaitingFor};
    use crate::util::runner::{FakeRunner, Output};

    /// A card of session `n` that has waited since `since` seconds.
    fn card(n: u8, since: i64, seen: Seen) -> InboxEntry {
        entry(n, crate::testutil::turn_card(n, at(since)), seen)
    }

    fn quota_hold(n: u8, since: i64) -> InboxEntry {
        let card = PendingCard::Blocked {
            session: session_id(n),
            on: WaitingFor::QuotaHold,
            since: at(since),
        };
        entry(n, card, Seen::Unseen)
    }

    fn entry(n: u8, card: PendingCard, seen: Seen) -> InboxEntry {
        InboxEntry {
            seen,
            ..crate::testutil::inbox_entry(n, card)
        }
    }

    fn model_with(cards: Vec<InboxEntry>) -> Model {
        let mut model = Model::unread(at(0));
        model.inbox = cards;
        model
    }

    fn ran_ok() -> Output {
        Output {
            exit: Exit::Success,
            stdout: Vec::new(),
            stderr: String::new(),
        }
    }

    /// The first unseen card notifies with the count of unseen cards, once: the same card seen
    /// poll after poll is not new, and a seen card is not counted.
    /// Mutation: count every entry (the message says 3), send the card's title or text instead
    /// of the count, or key on the session alone (the later test goes red).
    #[tokio::test]
    async fn an_unseen_card_sends_the_count_of_unseen_cards_once() {
        let runner = FakeRunner::new([Ok(ran_ok()), Ok(ran_ok())]);
        let mut notifier = Notifier::default();
        notifier.observe(&model_with(vec![]), &runner, at(1)).await;
        assert_eq!(runner.lines().len(), 0);

        let busy = model_with(vec![
            card(1, 10, Seen::Unseen),
            card(2, 11, Seen::Seen),
            card(3, 12, Seen::Unseen),
        ]);
        notifier.observe(&busy, &runner, at(2)).await;
        notifier.observe(&busy, &runner, at(3)).await;
        notifier.observe(&busy, &runner, at(200)).await;
        assert_eq!(runner.lines(), ["notify-send conn 2 sessions need you"]);
        assert_eq!(notifier.state(), &Notifications::Sent { at: at(2) });
    }

    /// A second new card while one is still unseen notifies once the debounce has passed, and
    /// carries the current count; a card inside the debounce waits for it rather than being
    /// lost, and a session's later turn end is a new card.
    /// Mutation: notify only when the count was zero before (the old crossing), drop the
    /// debounce (the 30 s row sends), or forget the waiting card (the 61 s row stays silent).
    #[tokio::test]
    async fn a_second_new_card_notifies_after_the_debounce_with_the_current_count() {
        let runner = FakeRunner::new([Ok(ran_ok()), Ok(ran_ok()), Ok(ran_ok())]);
        let mut notifier = Notifier::default();
        let one = model_with(vec![card(1, 0, Seen::Unseen)]);
        notifier.observe(&one, &runner, at(100)).await;

        let two = model_with(vec![card(1, 0, Seen::Unseen), card(2, 120, Seen::Unseen)]);
        notifier.observe(&two, &runner, at(130)).await;
        assert_eq!(runner.lines().len(), 1, "30 s after the last: debounced");
        notifier.observe(&two, &runner, at(159)).await;
        assert_eq!(runner.lines().len(), 1, "59 s after the last: debounced");
        notifier.observe(&two, &runner, at(161)).await;
        assert_eq!(
            runner.lines(),
            [
                "notify-send conn 1 sessions need you",
                "notify-send conn 2 sessions need you"
            ]
        );

        // The same session finishes another turn: a new card, so a notification a minute on.
        let three = model_with(vec![card(1, 0, Seen::Unseen), card(1, 200, Seen::Unseen)]);
        notifier.observe(&three, &runner, at(230)).await;
        assert_eq!(runner.lines().len(), 3);
    }

    /// A card that leaves the inbox for a poll and comes back with the same key is not new.
    /// Mutation: forget announced cards when they leave the inbox.
    #[tokio::test]
    async fn a_card_reappearing_with_the_same_key_does_not_notify() {
        let runner = FakeRunner::new([Ok(ran_ok()), Ok(ran_ok())]);
        let mut notifier = Notifier::default();
        let one = model_with(vec![card(1, 0, Seen::Unseen)]);
        notifier.observe(&one, &runner, at(10)).await;
        notifier
            .observe(&model_with(vec![]), &runner, at(100))
            .await;
        notifier.observe(&one, &runner, at(200)).await;
        assert_eq!(runner.lines().len(), 1);
    }

    /// A quota hold is a card on the page and never a notification, alone or beside others.
    /// Mutation: drop the tier filter in `unseen`.
    #[tokio::test]
    async fn a_quota_hold_never_notifies() {
        let runner = FakeRunner::new([Ok(ran_ok()), Ok(ran_ok())]);
        let mut notifier = Notifier::default();
        notifier
            .observe(&model_with(vec![quota_hold(1, 0)]), &runner, at(10))
            .await;
        assert_eq!(runner.lines().len(), 0);
        notifier
            .observe(
                &model_with(vec![quota_hold(1, 0), card(2, 20, Seen::Unseen)]),
                &runner,
                at(100),
            )
            .await;
        assert_eq!(runner.lines(), ["notify-send conn 1 sessions need you"]);
    }

    /// Mutation: retry a missing program on every card (the second card makes a call), or drop
    /// the `Missing` record.
    #[tokio::test]
    async fn a_missing_notify_send_is_recorded_once_and_never_asked_again() {
        let runner = FakeRunner::new([Err(RunError::NotFound)]);
        let mut notifier = Notifier::default();
        notifier
            .observe(&model_with(vec![card(1, 0, Seen::Unseen)]), &runner, at(1))
            .await;
        assert_eq!(notifier.state(), &Notifications::Missing { at: at(1) });
        notifier
            .observe(
                &model_with(vec![card(1, 0, Seen::Unseen), card(2, 50, Seen::Unseen)]),
                &runner,
                at(300),
            )
            .await;
        assert_eq!(runner.lines().len(), 1);
        assert_eq!(notifier.state(), &Notifications::Missing { at: at(1) });
    }

    /// A failing `notify-send` is shown and tried again once the debounce has passed, for the
    /// same cards.
    /// Mutation: map a non-zero exit to `Sent`, or announce the cards before the send succeeds.
    #[tokio::test]
    async fn a_failed_send_is_recorded_and_retried_after_the_debounce() {
        let failing = Ok(Output {
            exit: Exit::Code(1),
            stdout: Vec::new(),
            stderr: "no daemon".to_owned(),
        });
        let runner = FakeRunner::new([failing, Ok(ran_ok())]);
        let mut notifier = Notifier::default();
        let one = model_with(vec![card(1, 0, Seen::Unseen)]);
        notifier.observe(&one, &runner, at(1)).await;
        assert!(
            matches!(notifier.state(), Notifications::Failed { at: failed_at, reason }
                if *failed_at == at(1) && reason.contains("no daemon")),
            "{:?}",
            notifier.state()
        );
        notifier.observe(&one, &runner, at(30)).await;
        assert_eq!(runner.lines().len(), 1, "inside the debounce");
        notifier.observe(&one, &runner, at(70)).await;
        assert_eq!(notifier.state(), &Notifications::Sent { at: at(70) });
    }
}
