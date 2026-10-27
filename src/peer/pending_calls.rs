use std::{
    collections::{hash_map::Entry, HashMap},
    mem,
    num::NonZeroU32,
    sync::{Mutex, MutexGuard, PoisonError},
};

use zbus::names::OwnedUniqueName;

/// The method calls to a peer that wait for its reply: the caller and the serial of each.
///
/// The bus records each call that expects a reply as it passes the call on to its callee, as
/// dbus-broker does, and only passes on a reply, or an error, that takes the record of a call away:
/// one from the callee, to the caller, whose reply serial is the serial of the call. So a peer
/// can't pass a reply of its own off as the reply to a call that another peer made of a third one,
/// which a caller that tells replies apart by their serial alone would take it for. The records
/// also let the bus answer the calls that wait on a peer that goes, with an error.
///
/// The records of a peer are shared between the peer and the task that serves it, as the peer
/// goes, when it becomes a monitor, before its task does.
#[derive(Debug, Default)]
pub struct PendingCalls(Mutex<Calls>);

impl PendingCalls {
    /// Record the call of `caller`, whose number is `caller_id`, with `serial`, unless one with
    /// that serial waits already, in which case it's `false`.
    pub fn add(&self, caller_id: usize, caller: &OwnedUniqueName, serial: NonZeroU32) -> bool {
        match self.calls().entry((caller_id, serial)) {
            Entry::Occupied(_) => false,
            Entry::Vacant(call) => {
                call.insert(caller.clone());

                true
            }
        }
    }

    /// Take the record of the call of the caller whose number is `caller_id`, with `serial`, away,
    /// if there is one, in which case it's `true`.
    pub fn remove(&self, caller_id: usize, serial: NonZeroU32) -> bool {
        self.calls().remove(&(caller_id, serial)).is_some()
    }

    /// Forget the calls of the caller whose number is `caller_id`, which goes.
    pub fn forget(&self, caller_id: usize) {
        self.calls().retain(|(id, _), _| *id != caller_id);
    }

    /// Take all of the records away: the caller and the serial of each call.
    pub fn take(&self) -> impl Iterator<Item = (OwnedUniqueName, NonZeroU32)> {
        mem::take(&mut *self.calls())
            .into_iter()
            .map(|((_, serial), caller)| (caller, serial))
    }

    fn calls(&self) -> MutexGuard<'_, Calls> {
        // Nothing can panic while the lock is held.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The unique name of the caller of each call, by the number of the caller, as [`Peer::id`] has it,
/// and the serial of the call.
///
/// The records are hashed with foldhash rather than with the SipHash of the standard library, which
/// takes about 3 times as long with them: a record and its removal take about 200 instructions
/// rather than 530. A caller picks the serials of its calls, but foldhash is seeded at random, so
/// that it can't pick serials whose hashes collide.
///
/// [`Peer::id`]: super::Peer::id
type Calls = HashMap<(usize, NonZeroU32), OwnedUniqueName, foldhash::fast::RandomState>;
