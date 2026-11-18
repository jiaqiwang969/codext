//! Periodic and on-demand passes that drive due effects and, later, the engine's evaluations
//! (prototype `src/app/passes.ts`).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::watch;

use crate::BoxFut;
use crate::effects::Effects;
use crate::health::Health;
use crate::health::HealthBoard;
use crate::health::HealthEntry;

/// What a pass runs after kicking the effects: the engine's `evaluate_due`, once T-12 adds it.
pub type PassHook = Arc<dyn Fn() -> BoxFut<'static, anyhow::Result<()>> + Send + Sync>;

const INTERVAL: Duration = Duration::from_secs(2);

struct Inner {
    effects: Effects,
    health: HealthEntry,
    hook: Mutex<Option<PassHook>>,
    stopped: AtomicBool,
    /// A pass is running; a request meanwhile sets `again`, and the running pass runs once more.
    pumping: AtomicBool,
    again: AtomicBool,
    /// Bumps when a pass ends, for `idle`.
    pumped: watch::Sender<u64>,
}

#[derive(Clone)]
pub struct Passes {
    inner: Arc<Inner>,
}

impl Passes {
    pub fn new(effects: Effects, board: HealthBoard) -> anyhow::Result<Self> {
        let health = board.claim(
            "runtime",
            Some(Health {
                state: crate::health::HealthState::Starting,
                detail: "Starting".to_owned(),
                at: None,
            }),
        )?;
        let (pumped, _) = watch::channel(0u64);
        Ok(Self {
            inner: Arc::new(Inner {
                effects,
                health,
                hook: Mutex::new(None),
                stopped: AtomicBool::new(false),
                pumping: AtomicBool::new(false),
                again: AtomicBool::new(false),
                pumped,
            }),
        })
    }

    /// Installs what each pass runs after the effects; replaces an earlier hook.
    pub fn set_hook(&self, hook: PassHook) {
        *self
            .inner
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// Starts the two-second ticker.
    pub fn start(&self) {
        let passes = self.clone();
        passes.inner.health.set(Health::ready(
            "Orchestrator is ready",
            Some(crate::core::now_iso()),
        ));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if passes.inner.stopped.load(Ordering::SeqCst) {
                    return;
                }
                passes.request();
            }
        });
    }

    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
    }

    /// Asks for a pass now, without waiting for it.
    pub fn request(&self) {
        let passes = self.clone();
        tokio::spawn(async move {
            passes.pump().await;
        });
    }

    /// Runs one pass, coalescing concurrent callers into a single follow-up pass.
    pub async fn pump(&self) {
        if self.inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        if self.inner.pumping.swap(true, Ordering::SeqCst) {
            self.inner.again.store(true, Ordering::SeqCst);
            return;
        }
        loop {
            self.inner.again.store(false, Ordering::SeqCst);
            self.pump_once().await;
            if self.inner.again.swap(false, Ordering::SeqCst) {
                continue;
            }
            self.inner.pumping.store(false, Ordering::SeqCst);
            // A request between that check and this release would be lost without this second look.
            if self.inner.again.load(Ordering::SeqCst)
                && !self.inner.pumping.swap(true, Ordering::SeqCst)
            {
                continue;
            }
            break;
        }
        self.inner.pumped.send_modify(|count| *count += 1);
    }

    async fn pump_once(&self) {
        self.inner.effects.kick();
        let hook = self
            .inner
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let result = match hook {
            Some(hook) => hook().await,
            None => Ok(()),
        };
        let at = Some(crate::core::now_iso());
        match result {
            Ok(()) => {
                if self
                    .inner
                    .health
                    .get()
                    .is_some_and(|h| h.state == crate::health::HealthState::Error)
                {
                    self.inner
                        .health
                        .set(Health::ready("Orchestrator is ready", at));
                }
            }
            Err(err) => self
                .inner
                .health
                .set(Health::error(format!("A pass failed: {err:#}"), at)),
        }
    }

    /// Waits until no pass and no effect that can start now is running.
    pub async fn idle(&self) {
        let mut pumped = self.inner.pumped.subscribe();
        loop {
            let seen = *pumped.borrow_and_update();
            if self.inner.pumping.load(Ordering::SeqCst) {
                let _ = pumped.wait_for(|count| *count != seen).await;
                continue;
            }
            self.inner.effects.idle(None).await;
            if !self.inner.pumping.load(Ordering::SeqCst)
                && !self.inner.again.load(Ordering::SeqCst)
            {
                return;
            }
        }
    }
}
