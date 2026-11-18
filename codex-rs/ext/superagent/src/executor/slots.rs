//! Worker slots (prototype `dsh.ts` `WorkerSlots`, with the design's three layers, §6.6): a ticket
//! holds a slot from the start of a launch or from the delivery that wakes its idle worker, and
//! throughout an active turn. Wakes take the next free slot before launches, and a person's
//! message may use one slot beyond the limit. Full slots queue; they never error.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotBy {
    Launch,
    Wake,
    Person,
}

#[derive(Clone, Debug)]
struct Held {
    pm: Option<String>,
}

#[derive(Debug)]
struct Inner {
    max_global: usize,
    max_per_pm: usize,
    held: HashMap<String, Held>,
    /// Wakes that found no room; the next free slot is theirs before any launch.
    wakes_waiting: usize,
}

#[derive(Debug)]
pub struct WorkerSlots {
    inner: Mutex<Inner>,
}

impl WorkerSlots {
    pub fn new(max_global: usize, max_per_pm: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                max_global,
                max_per_pm,
                held: HashMap::new(),
                wakes_waiting: 0,
            }),
        }
    }

    /// Reserves a slot for a ticket, or answers false when none is free for this kind of claim.
    pub fn reserve(&self, ticket_id: &str, pm: Option<&str>, by: SlotBy) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.held.contains_key(ticket_id) {
            return true;
        }
        let used = inner.held.len();
        let pm_used = pm.map_or(0, |pm| {
            inner
                .held
                .values()
                .filter(|held| held.pm.as_deref() == Some(pm))
                .count()
        });
        let extra = usize::from(by == SlotBy::Person);
        let room = used < inner.max_global + extra && pm_used < inner.max_per_pm + extra;
        let allowed = match by {
            SlotBy::Launch => room && inner.wakes_waiting == 0,
            SlotBy::Wake | SlotBy::Person => room,
        };
        if !allowed {
            if by == SlotBy::Wake {
                inner.wakes_waiting += 1;
            }
            return false;
        }
        if by == SlotBy::Wake && inner.wakes_waiting > 0 {
            inner.wakes_waiting -= 1;
        }
        inner.held.insert(
            ticket_id.to_owned(),
            Held {
                pm: pm.map(str::to_owned),
            },
        );
        true
    }

    pub fn free(&self, ticket_id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .held
            .remove(ticket_id);
    }

    pub fn holds(&self, ticket_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .held
            .contains_key(ticket_id)
    }

    pub fn used(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .held
            .len()
    }

    /// How many launches may start now: the free slots, none while a wake waits.
    pub fn launch_room(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.wakes_waiting > 0 {
            return 0;
        }
        inner.max_global.saturating_sub(inner.held.len())
    }
}

