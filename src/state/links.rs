//! What is derived from a session's prompts, once per change of its transcript: the links it
//! mentions and the redacted prompts that search reads.
//!
//! Both read up to fifty prompts of up to 32 KiB each, and the model is rebuilt every two
//! seconds for every session, so the result is kept until the inputs change. The memo is an
//! optimisation: removing it changes the speed of a rebuild and nothing it produces.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use crate::links::{Extracted, Extractor};
use crate::model::HeldPrompt;
use crate::types::{Instant, LinkTargets, SessionId};

/// What a session's links are derived from changes only when one of these does: the bytes of
/// its transcript, the time of its latest record, and the time of its latest Stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub bytes_read: u64,
    pub last_record_at: Option<Instant>,
    pub last_stop_at: Option<Instant>,
}

/// What one session's transcript yields, kept together because it changes together.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Derived {
    pub extracted: Extracted,
    /// The human prompts, oldest first, redacted.
    pub prompts: Arc<[HeldPrompt]>,
}

#[derive(Debug, Default)]
pub struct DerivedMemo {
    entries: Mutex<HashMap<SessionId, (Fingerprint, Arc<Derived>)>>,
}

impl DerivedMemo {
    pub fn get_or_derive(
        &self,
        session: &SessionId,
        fingerprint: Fingerprint,
        derive: impl FnOnce() -> Derived,
    ) -> Arc<Derived> {
        let known = {
            let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            entries
                .get(session)
                .filter(|(kept, _)| *kept == fingerprint)
                .map(|(_, extracted)| Arc::clone(extracted))
        };
        known.unwrap_or_else(|| {
            let fresh = Arc::new(derive());
            self.entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(session.clone(), (fingerprint, Arc::clone(&fresh)));
            fresh
        })
    }

    /// Forgets sessions that are no longer known.
    pub fn retain(&self, keep: &HashSet<&SessionId>) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|session, _| keep.contains(session));
    }
}

/// What extraction needs from outside the transcript.
#[derive(Clone, Copy)]
pub struct LinkEnv<'a> {
    pub extractor: &'a Extractor,
    pub targets: &'a LinkTargets,
    pub memo: &'a DerivedMemo,
}
