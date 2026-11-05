//! The host interface: what the engine asks app-server to do with codext threads (design §1.2).
//!
//! T-11 defined the contract; T-14 added what the executor needs (background commands, goal
//! status). The app-server implementation lives in `app-server/src/superagent_host.rs` and reuses
//! the existing request processors: thread/start, account/profile/switch, thread/resume, the
//! persistent queue, turn/steer, Op::Interrupt, thread/backgroundTerminals/clean, the
//! `thread_spawn_edges` table and thread/goal.

use std::path::PathBuf;

use codex_protocol::ThreadId;
use serde::Serialize;

use crate::BoxFut;

/// How to create a worker's root thread.
#[derive(Clone, Debug, Default)]
pub struct ThreadSpec {
    pub cwd: PathBuf,
    /// The account profile the thread runs under (`account/profile/switch` with `threadId`).
    pub profile: Option<String>,
    pub model: Option<String>,
    /// The role file to apply, such as `worker`.
    pub role: Option<String>,
    pub developer_instructions: Option<String>,
}

/// Whether an input waits for the current turn to end or is steered into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryMode {
    Queue,
    Steer,
}

/// A thread's Goal, as the executor reads it: only the budget states matter to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalStatus {
    Active,
    Completed,
    Stopped,
    BudgetLimited,
    UsageLimited,
    Other(String),
}

impl GoalStatus {
    /// Whether the worker cannot go on until a person adds budget or the limit resets.
    pub fn exhausted(&self) -> bool {
        matches!(self, Self::BudgetLimited | Self::UsageLimited)
    }
}

/// A session as the Router's session index shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBrief {
    pub id: String,
    pub name: Option<String>,
    pub preview: String,
    pub cwd: String,
    /// The thread status type, with its active flags after a colon when it has any
    /// (`active:waitingOnApproval`).
    pub status: String,
    pub updated_at: Option<i64>,
    /// The model and reasoning effort the session last ran with.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub account_profile: Option<String>,
}

/// One page of sessions, newest first, and where the next page starts.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    pub sessions: Vec<SessionBrief>,
    pub next_cursor: Option<String>,
}

/// One turn of a session: what the person asked and what the assistant answered.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnBrief {
    pub id: String,
    pub status: String,
    pub user: Vec<String>,
    pub assistant: Vec<String>,
    /// Why a failed turn failed, as the person saw it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The error's kind (`codexErrorInfo`, such as `usageLimitExceeded` or `serverOverloaded`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

/// Drives threads through the host's normal admission, account, and lifecycle checks.
/// Implementations must report failed delivery instead of silently creating another thread.
pub trait SuperagentHost: Send + Sync {
    /// Creates a root thread exactly as an external client would.
    fn start_thread(&self, spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>>;
    /// Binds an account profile to a thread; refused while the thread or its children work.
    fn bind_profile(&self, thread: ThreadId, profile: String) -> BoxFut<'_, anyhow::Result<()>>;
    /// Changes the model and reasoning effort a loaded thread uses from its next turn on;
    /// `None` for both means back to the role file `role` names (design §6.8).
    fn set_thread_model(
        &self,
        thread: ThreadId,
        role: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        let _ = (role, model, effort);
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "this host cannot change the model of thread {thread}"
            ))
        })
    }
    /// Loads a cold thread before delivering to it.
    /// `role` names the role file to apply on a cold load, so a resumed worker follows the
    /// current file (model, effort, instructions) rather than the one it was created with;
    /// `None` keeps the thread's own settings (PM threads).
    fn resume_thread(
        &self,
        thread: ThreadId,
        role: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>>;
    /// Hands text to a thread: queued for its next turn, or steered into the running one.
    fn deliver(
        &self,
        thread: ThreadId,
        text: String,
        mode: DeliveryMode,
    ) -> BoxFut<'_, anyhow::Result<()>>;
    /// Starts a drive step on an idle session: a new turn with the drive's own settings (full
    /// access and no approvals, as the person decided, so the worker can open applications and
    /// browsers) and the `drive` trigger, whatever the session ran with before. A host without
    /// per-turn settings steers it.
    fn start_step(&self, thread: ThreadId, text: String) -> BoxFut<'_, anyhow::Result<()>> {
        self.deliver(thread, text, DeliveryMode::Steer)
    }
    /// One page of the host's root sessions, newest first, read from stored metadata without
    /// loading any thread. A host without a session store has none.
    fn list_sessions(
        &self,
        cursor: Option<String>,
        limit: u32,
    ) -> BoxFut<'_, anyhow::Result<SessionPage>> {
        let _ = (cursor, limit);
        Box::pin(async { Ok(SessionPage::default()) })
    }
    /// The account profile a thread runs under, when the host knows it.
    fn thread_profile(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<String>>> {
        let _ = thread;
        Box::pin(async { Ok(None) })
    }
    /// A session's latest turns, newest first, read from its history without resuming it.
    fn session_turns(
        &self,
        thread: ThreadId,
        limit: u32,
    ) -> BoxFut<'_, anyhow::Result<Vec<TurnBrief>>> {
        let _ = (thread, limit);
        Box::pin(async { Ok(Vec::new()) })
    }
    /// The items waiting in the thread's persistent queue, as (id, text), oldest first. A host
    /// without a persistent queue has none.
    fn queued(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<(String, String)>>> {
        let _ = thread;
        Box::pin(async { Ok(Vec::new()) })
    }
    /// Removes one item from the thread's persistent queue; answers whether it was still there.
    fn withdraw_queued(
        &self,
        thread: ThreadId,
        item_id: String,
    ) -> BoxFut<'_, anyhow::Result<bool>> {
        let _ = (thread, item_id);
        Box::pin(async { Ok(false) })
    }
    /// Interrupts the current turn.
    fn interrupt(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>>;
    /// Ends the thread's background commands.
    fn clean_background(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>>;
    /// The thread's background commands still running, one label each.
    fn background_commands(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<String>>>;
    /// The subagent threads a worker spawned that are still running (`thread_spawn_edges`).
    fn children(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<ThreadId>>>;
    /// Sets a token budget as the thread's goal, or clears it.
    fn goal_set(
        &self,
        thread: ThreadId,
        objective: String,
        token_budget: Option<i64>,
    ) -> BoxFut<'_, anyhow::Result<()>>;
    fn goal_clear(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>>;
    /// The thread's Goal status, if it has a Goal.
    fn goal_status(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<GoalStatus>>>;
    /// The tokens the thread has used so far (codext's own count, cached input included), when the
    /// host keeps one. The executor's per-assignment budget is the difference since the assignment
    /// started.
    fn tokens_used(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<i64>>> {
        let _ = thread;
        Box::pin(async { Ok(None) })
    }
}

pub use ledger::DeliveryLedger;
pub use ledger::LedgerHost;

mod ledger {
    use std::collections::VecDeque;
    use std::hash::DefaultHasher;
    use std::hash::Hash;
    use std::hash::Hasher;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::PoisonError;

    use codex_protocol::ThreadId;

    use super::DeliveryMode;
    use super::GoalStatus;
    use super::SessionPage;
    use super::SuperagentHost;
    use super::ThreadSpec;
    use super::TurnBrief;
    use crate::BoxFut;

    /// What the engine itself handed to threads, by text hash, so the intake never reads an engine
    /// delivery (a candidate card, a relay, an assignment) back as a person's message. In memory and
    /// bounded: after a restart, a card the persistent queue still held yields one spurious trigger,
    /// whose own card is then recognised, so there is no loop.
    #[derive(Default)]
    pub struct DeliveryLedger {
        recent: Mutex<VecDeque<u64>>,
    }

    impl DeliveryLedger {
        const CAPACITY: usize = 4096;

        fn hash(text: &str) -> u64 {
            let mut hasher = DefaultHasher::new();
            text.trim().hash(&mut hasher);
            hasher.finish()
        }

        pub fn note(&self, text: &str) {
            let hash = Self::hash(text);
            let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
            if recent.len() >= Self::CAPACITY {
                recent.pop_front();
            }
            recent.push_back(hash);
        }

        pub fn delivered(&self, text: &str) -> bool {
            let hash = Self::hash(text);
            self.recent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&hash)
        }
    }

    /// The host as the engine uses it: every delivery is noted in the ledger first.
    pub struct LedgerHost {
        inner: Arc<dyn SuperagentHost>,
        ledger: Arc<DeliveryLedger>,
    }

    impl LedgerHost {
        pub fn new(inner: Arc<dyn SuperagentHost>, ledger: Arc<DeliveryLedger>) -> Self {
            Self { inner, ledger }
        }
    }

    impl SuperagentHost for LedgerHost {
        fn start_thread(&self, spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
            self.inner.start_thread(spec)
        }

        fn bind_profile(
            &self,
            thread: ThreadId,
            profile: String,
        ) -> BoxFut<'_, anyhow::Result<()>> {
            self.inner.bind_profile(thread, profile)
        }

        fn resume_thread(
            &self,
            thread: ThreadId,
            role: Option<String>,
        ) -> BoxFut<'_, anyhow::Result<()>> {
            self.inner.resume_thread(thread, role)
        }

        fn set_thread_model(
            &self,
            thread: ThreadId,
            role: Option<String>,
            model: Option<String>,
            effort: Option<String>,
        ) -> BoxFut<'_, anyhow::Result<()>> {
            self.inner.set_thread_model(thread, role, model, effort)
        }

        fn deliver(
            &self,
            thread: ThreadId,
            text: String,
            mode: DeliveryMode,
        ) -> BoxFut<'_, anyhow::Result<()>> {
            self.ledger.note(&text);
            self.inner.deliver(thread, text, mode)
        }

        fn start_step(&self, thread: ThreadId, text: String) -> BoxFut<'_, anyhow::Result<()>> {
            self.ledger.note(&text);
            self.inner.start_step(thread, text)
        }

        fn list_sessions(
            &self,
            cursor: Option<String>,
            limit: u32,
        ) -> BoxFut<'_, anyhow::Result<SessionPage>> {
            self.inner.list_sessions(cursor, limit)
        }

        fn thread_profile(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<String>>> {
            self.inner.thread_profile(thread)
        }

        fn session_turns(
            &self,
            thread: ThreadId,
            limit: u32,
        ) -> BoxFut<'_, anyhow::Result<Vec<TurnBrief>>> {
            self.inner.session_turns(thread, limit)
        }

        fn queued(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<(String, String)>>> {
            self.inner.queued(thread)
        }

        fn withdraw_queued(
            &self,
            thread: ThreadId,
            item_id: String,
        ) -> BoxFut<'_, anyhow::Result<bool>> {
            self.inner.withdraw_queued(thread, item_id)
        }

        fn interrupt(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
            self.inner.interrupt(thread)
        }

        fn clean_background(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
            self.inner.clean_background(thread)
        }

        fn background_commands(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<String>>> {
            self.inner.background_commands(thread)
        }

        fn children(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<ThreadId>>> {
            self.inner.children(thread)
        }

        fn goal_set(
            &self,
            thread: ThreadId,
            objective: String,
            token_budget: Option<i64>,
        ) -> BoxFut<'_, anyhow::Result<()>> {
            self.inner.goal_set(thread, objective, token_budget)
        }

        fn goal_clear(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
            self.inner.goal_clear(thread)
        }

        fn goal_status(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<GoalStatus>>> {
            self.inner.goal_status(thread)
        }

        fn tokens_used(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<i64>>> {
            self.inner.tokens_used(thread)
        }
    }

}
