//! Capacity fallback and bounded continuation of a specific failed assignment turn.

use super::*;
use sha2::Digest;
use sha2::Sha256;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Continuation {
    ticket_id: String,
    assignment_id: String,
    thread_id: String,
    turn_id: String,
    attempt: u32,
    ticket_fingerprint: String,
}

fn fingerprint(ticket: &Ticket) -> anyhow::Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(ticket)?)))
}

impl CodextThreadExecutor {
    /// Capacity/limit incidents of a thread within the policy window (design §6.8).
    fn recent_incidents(&self, thread_id: &str) -> usize {
        let window = chrono::Duration::from_std(self.inner.config.capacity_window)
            .unwrap_or_else(|_| chrono::Duration::seconds(600));
        let since = chrono::Utc::now() - window;
        crate::incidents::recent()
            .iter()
            .filter(|incident| incident.thread == thread_id)
            .filter(|incident| {
                chrono::DateTime::parse_from_rfc3339(&incident.at)
                    .map(|at| at >= since)
                    .unwrap_or(false)
            })
            .count()
    }

    /// A capacity incident on a thread: a worker past the threshold goes on the fallback model
    /// until the restore, which is scheduled here.
    pub async fn on_incident(&self, thread_id: &str) {
        let Some(fallback) = self.inner.config.capacity_fallback_model.clone() else {
            return;
        };
        if self.recent_incidents(thread_id) < self.inner.config.capacity_threshold {
            return;
        }
        let is_worker = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|record| record.thread_id == thread_id);
        if !is_worker {
            return;
        }
        {
            let mut fallbacks = self
                .inner
                .fallbacks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if fallbacks.contains_key(thread_id) {
                return;
            }
            fallbacks.insert(thread_id.to_owned(), Instant::now());
        }
        let Ok(thread) = ThreadId::from_string(thread_id) else {
            return;
        };
        let role = Some(self.inner.config.worker_role.clone());
        let effort = self.inner.config.capacity_fallback_effort.clone();
        match self
            .inner
            .host
            .set_thread_model(thread, role, Some(fallback.clone()), effort)
            .await
        {
            Ok(()) => tracing::warn!(
                "superagent: thread {thread_id} falls back to {fallback} after capacity incidents"
            ),
            Err(err) => {
                tracing::warn!("superagent: fallback of {thread_id} failed: {err:#}");
                self.inner
                    .fallbacks
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(thread_id);
                return;
            }
        }
        let payload = serde_json::json!({ "threadId": thread_id });
        if let Err(err) = self
            .inner
            .effects
            .enqueue_after(
                KIND_MODEL_RESTORE,
                thread_id,
                &payload,
                self.inner.config.capacity_restore,
            )
            .await
        {
            tracing::warn!("superagent: could not schedule the restore of {thread_id}: {err:#}");
        }
    }

    /// Back on the role model once the window passed without incidents; else later again.
    pub async fn restore_model(&self, thread_id: &str) -> Result<(), EffectError> {
        if self.recent_incidents(thread_id) > 0 {
            return Err(EffectError::Later("capacity incidents continue".to_owned()));
        }
        let Ok(thread) = ThreadId::from_string(thread_id) else {
            return Ok(());
        };
        let role = Some(self.inner.config.worker_role.clone());
        self.inner
            .host
            .set_thread_model(thread, role, None, None)
            .await
            .map_err(|err| EffectError::Failed(format!("restore of {thread_id}: {err:#}")))?;
        self.inner
            .fallbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(thread_id);
        tracing::warn!("superagent: thread {thread_id} is back on its role model");
        Ok(())
    }

    /// Capture the exact failed turn and ticket revision. Counter, receipt, and delayed
    /// effect commit together; restarting the app-server cannot reset the assignment budget.
    pub(super) async fn schedule_continue(&self, thread_id: &str, state: &TurnState) {
        if self.inner.config.capacity_continue_max == 0 {
            return;
        }
        let result = self.inner.store.atomic(async |tx| {
            let Some(ticket) = tx.ticket(&state.ticket_id).await? else { return Ok(()); };
            let Some(assignment) = latest_assignment(&ticket) else { return Ok(()); };
            if ticket.ended.is_some() || ticket.attention.is_some()
                || !ticket.undelivered.is_empty() || assignment.id != state.assignment_id
                || assignment.state != AssignmentState::Running
            {
                return Ok(());
            }
            let receipt = format!("capacity-scheduled:{}:{}", state.assignment_id, state.turn_id);
            if tx.kv_get::<bool>(&receipt).await?.unwrap_or(false) {
                return Ok(());
            }
            let counter = format!("capacity-continue-count:{}", state.assignment_id);
            let count = tx.kv_get::<u32>(&counter).await?.unwrap_or(0);
            if count >= self.inner.config.capacity_continue_max {
                return Ok(());
            }
            let continuation = Continuation {
                ticket_id: state.ticket_id.clone(),
                assignment_id: state.assignment_id.clone(),
                thread_id: thread_id.to_owned(),
                turn_id: state.turn_id.clone(),
                attempt: count + 1,
                ticket_fingerprint: fingerprint(&ticket)?,
            };
            tx.kv_set(&counter, &continuation.attempt).await?;
            tx.kv_set(&receipt, &true).await?;
            self.inner.effects.enqueue_after_in(
                tx, KIND_CONTINUE, &state.assignment_id, &continuation,
                self.inner.config.capacity_continue_delay,
            ).await
        }).await;
        if let Err(error) = result {
            tracing::warn!("superagent: could not schedule continuation for {}: {error:#}", state.ticket_id);
        }
    }

    /// Discard stale work after any new input, turn, report, stop, or reassignment.
    /// Delivery and its consumed receipt commit together, so an effect retry is a no-op.
    pub(super) async fn continue_after_incident(
        &self,
        continuation: Continuation,
    ) -> Result<(), EffectError> {
        let _lane = self.lane(&continuation.ticket_id).acquire_owned().await
            .map_err(|error| EffectError::Failed(error.to_string()))?;
        if self.inner.paused.load(Ordering::SeqCst) || self.turn_running(&continuation.thread_id) {
            return Ok(());
        }
        let Some(ticket) = self.inner.store.ticket(&continuation.ticket_id).await? else {
            return Ok(());
        };
        if fingerprint(&ticket)? != continuation.ticket_fingerprint {
            return Ok(());
        }
        let thread = ThreadId::from_string(&continuation.thread_id)
            .map_err(|error| EffectError::Failed(error.to_string()))?;
        self.inner.host.resume_thread(thread, Some(self.inner.config.worker_role.clone())).await?;
        if !self.inner.host.children(thread).await?.is_empty()
            || !self.inner.host.background_commands(thread).await?.is_empty()
            || self.inflight_for(&continuation.thread_id).await?.is_some()
        {
            return Ok(());
        }
        let engine = Arc::clone(&self.inner.engine);
        self.inner.store.atomic(async |tx| {
            let Some(ticket) = tx.ticket(&continuation.ticket_id).await? else { return Ok(()); };
            let Some(assignment) = latest_assignment(&ticket) else { return Ok(()); };
            if ticket.ended.is_some() || ticket.attention.is_some()
                || !ticket.undelivered.is_empty() || assignment.id != continuation.assignment_id
                || assignment.state != AssignmentState::Running
                || fingerprint(&ticket)? != continuation.ticket_fingerprint
                || self.turn_running(&continuation.thread_id)
            {
                return Ok(());
            }
            let session: Option<SessionRecord> = tx.kv_get(&session_key(&continuation.ticket_id)).await?;
            if session.as_ref().is_none_or(|record| record.thread_id != continuation.thread_id) {
                return Ok(());
            }
            let receipt = format!("capacity-consumed:{}:{}", continuation.assignment_id, continuation.turn_id);
            if tx.kv_get::<bool>(&receipt).await?.unwrap_or(false) {
                return Ok(());
            }
            let attempt = continuation.attempt;
            let text = format!(
                "[系统] 上一轮因模型容量或额度错误终止，这是第 {attempt} 次自动续起：接着做，不必重述已完成的部分。"
            );
            engine.note(tx, &continuation.ticket_id, &text).await?;
            tx.kv_set(&receipt, &true).await
        }).await.map_err(EffectError::from)
    }
}
