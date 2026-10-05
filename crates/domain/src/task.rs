use crate::{EngineError, ToolExecutionResult, ToolVerificationOutcome};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, error::Error, fmt};

pub const MAX_TASK_ID_BYTES: usize = 128;
pub const MAX_TASK_REQUEST_BYTES: usize = 4 * 1024;
pub const MAX_TASK_STEP_BYTES: usize = 1024;
pub const MAX_TASK_REASON_BYTES: usize = 1024;
pub const MAX_TASK_STEPS_HARD: usize = 64;
pub const MAX_TASK_OBSERVATIONS_HARD: usize = 64;
pub const MAX_TASK_RETRIES_HARD: u32 = 16;
pub const MAX_TASK_REPLANS_HARD: u32 = 16;
pub const MAX_TASK_LIFETIME_MS_HARD: u64 = 24 * 60 * 60 * 1_000;
pub const MAX_TASK_PENDING_TTL_MS_HARD: u64 = 60 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(String);

impl TaskId {
    pub fn new(value: impl Into<String>) -> Result<Self, TaskError> {
        let value = value.into();
        validate_text("task_id", &value, MAX_TASK_ID_BYTES)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskLimits {
    pub max_steps: usize,
    pub max_observations: usize,
    pub max_retries: u32,
    pub max_replans: u32,
    pub max_lifetime_ms: u64,
    pub max_pending_ttl_ms: u64,
}

impl Default for TaskLimits {
    fn default() -> Self {
        Self {
            max_steps: 16,
            max_observations: 8,
            max_retries: 2,
            max_replans: 2,
            max_lifetime_ms: 15 * 60 * 1_000,
            max_pending_ttl_ms: 5 * 60 * 1_000,
        }
    }
}

impl TaskLimits {
    pub fn validate(self) -> Result<Self, TaskError> {
        if self.max_steps == 0 || self.max_steps > MAX_TASK_STEPS_HARD {
            return Err(TaskError::InvalidConfiguration("max_steps"));
        }
        if self.max_observations == 0 || self.max_observations > MAX_TASK_OBSERVATIONS_HARD {
            return Err(TaskError::InvalidConfiguration("max_observations"));
        }
        if self.max_retries > MAX_TASK_RETRIES_HARD {
            return Err(TaskError::InvalidConfiguration("max_retries"));
        }
        if self.max_replans > MAX_TASK_REPLANS_HARD {
            return Err(TaskError::InvalidConfiguration("max_replans"));
        }
        if self.max_lifetime_ms == 0 || self.max_lifetime_ms > MAX_TASK_LIFETIME_MS_HARD {
            return Err(TaskError::InvalidConfiguration("max_lifetime_ms"));
        }
        if self.max_pending_ttl_ms == 0
            || self.max_pending_ttl_ms > MAX_TASK_PENDING_TTL_MS_HARD
            || self.max_pending_ttl_ms > self.max_lifetime_ms
        {
            return Err(TaskError::InvalidConfiguration("max_pending_ttl_ms"));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStepState {
    Pending,
    Executing,
    Executed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStep {
    index: usize,
    description: String,
    state: TaskStepState,
}

impl TaskStep {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn state(&self) -> TaskStepState {
        self.state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingCommitmentKind {
    Approval,
    Dependency,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCommitment {
    kind: PendingCommitmentKind,
    summary: String,
    expires_at_ms: u64,
}

impl PendingCommitment {
    pub fn kind(&self) -> PendingCommitmentKind {
        self.kind
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskVerificationOutcome {
    Verified,
    Failed,
    NotVerified,
    Cancelled,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationResult {
    outcome: TaskVerificationOutcome,
    reason: Option<String>,
}

impl VerificationResult {
    pub fn verified() -> Self {
        Self {
            outcome: TaskVerificationOutcome::Verified,
            reason: None,
        }
    }

    pub fn failed(reason: impl Into<String>) -> Result<Self, TaskError> {
        Self::with_reason(TaskVerificationOutcome::Failed, reason)
    }

    pub fn not_verified(reason: impl Into<String>) -> Result<Self, TaskError> {
        Self::with_reason(TaskVerificationOutcome::NotVerified, reason)
    }

    pub fn cancelled(reason: impl Into<String>) -> Result<Self, TaskError> {
        Self::with_reason(TaskVerificationOutcome::Cancelled, reason)
    }

    pub fn stale(reason: impl Into<String>) -> Result<Self, TaskError> {
        Self::with_reason(TaskVerificationOutcome::Stale, reason)
    }

    fn with_reason(
        outcome: TaskVerificationOutcome,
        reason: impl Into<String>,
    ) -> Result<Self, TaskError> {
        let reason = reason.into();
        validate_text("verification_reason", &reason, MAX_TASK_REASON_BYTES)?;
        Ok(Self {
            outcome,
            reason: Some(reason),
        })
    }

    pub fn outcome(&self) -> TaskVerificationOutcome {
        self.outcome
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    Completed,
    Failed { reason: String },
    Cancelled { reason: String },
    NotVerified { reason: String },
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Accepted,
    Planning,
    Ready,
    Waiting(PendingCommitment),
    Executing { step_index: usize, generation: u64 },
    Verifying { generation: u64 },
    Completed,
    Failed,
    Cancelled,
    NotVerified,
    Expired,
}

impl TaskState {
    pub fn kind(&self) -> TaskStateKind {
        match self {
            Self::Accepted => TaskStateKind::Accepted,
            Self::Planning => TaskStateKind::Planning,
            Self::Ready => TaskStateKind::Ready,
            Self::Waiting(commitment) => match commitment.kind {
                PendingCommitmentKind::Approval => TaskStateKind::WaitingApproval,
                PendingCommitmentKind::Dependency => TaskStateKind::WaitingDependency,
            },
            Self::Executing { .. } => TaskStateKind::Executing,
            Self::Verifying { .. } => TaskStateKind::Verifying,
            Self::Completed => TaskStateKind::Completed,
            Self::Failed => TaskStateKind::Failed,
            Self::Cancelled => TaskStateKind::Cancelled,
            Self::NotVerified => TaskStateKind::NotVerified,
            Self::Expired => TaskStateKind::Expired,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStateKind {
    Accepted,
    Planning,
    Ready,
    WaitingApproval,
    WaitingDependency,
    Executing,
    Verifying,
    Completed,
    Failed,
    Cancelled,
    NotVerified,
    Expired,
}

impl TaskStateKind {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::NotVerified | Self::Expired
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTransitionReason {
    PlanningStarted,
    PlanReady,
    ApprovalPending,
    DependencyPending,
    CommitmentSatisfied,
    StepStarted,
    StepFinished,
    RetryScheduled,
    ReplanStarted,
    VerificationPassed,
    VerificationFailed,
    VerificationNotProven,
    VerificationCancelled,
    VerificationStale,
    Cancelled,
    TaskExpired,
    CommitmentExpired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTransition {
    pub task_id: TaskId,
    pub from: TaskStateKind,
    pub to: TaskStateKind,
    pub reason: TaskTransitionReason,
    pub at_ms: u64,
    pub generation: u64,
    pub retries: u32,
    pub replans: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskObservation {
    step_index: usize,
    generation: u64,
    result: ToolExecutionResult,
    verification: ToolVerificationOutcome,
}

impl TaskObservation {
    pub fn step_index(&self) -> usize {
        self.step_index
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn result(&self) -> &ToolExecutionResult {
        &self.result
    }

    pub fn verification(&self) -> ToolVerificationOutcome {
        self.verification
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentTask {
    id: TaskId,
    requested_outcome: String,
    state: TaskState,
    steps: Vec<TaskStep>,
    observations: VecDeque<TaskObservation>,
    limits: TaskLimits,
    created_at_ms: u64,
    updated_at_ms: u64,
    expires_at_ms: u64,
    generation: u64,
    retries: u32,
    replans: u32,
    outcome: Option<TaskOutcome>,
}

impl AgentTask {
    pub fn new(
        id: TaskId,
        requested_outcome: impl Into<String>,
        created_at_ms: u64,
        limits: TaskLimits,
    ) -> Result<Self, TaskError> {
        let limits = limits.validate()?;
        let requested_outcome = requested_outcome.into();
        validate_text(
            "requested_outcome",
            &requested_outcome,
            MAX_TASK_REQUEST_BYTES,
        )?;
        Ok(Self {
            id,
            requested_outcome,
            state: TaskState::Accepted,
            steps: Vec::new(),
            observations: VecDeque::new(),
            limits,
            created_at_ms,
            updated_at_ms: created_at_ms,
            expires_at_ms: created_at_ms.saturating_add(limits.max_lifetime_ms),
            generation: 0,
            retries: 0,
            replans: 0,
            outcome: None,
        })
    }

    pub fn id(&self) -> &TaskId {
        &self.id
    }

    pub fn requested_outcome(&self) -> &str {
        &self.requested_outcome
    }

    pub fn state(&self) -> &TaskState {
        &self.state
    }

    pub fn steps(&self) -> &[TaskStep] {
        &self.steps
    }

    pub fn observations(&self) -> &VecDeque<TaskObservation> {
        &self.observations
    }

    pub fn last_verified_observation(&self) -> Option<&TaskObservation> {
        self.observations
            .iter()
            .rev()
            .find(|observation| observation.verification == ToolVerificationOutcome::Verified)
    }

    pub fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    pub fn updated_at_ms(&self) -> u64 {
        self.updated_at_ms
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn retries(&self) -> u32 {
        self.retries
    }

    pub fn replans(&self) -> u32 {
        self.replans
    }

    pub fn outcome(&self) -> Option<&TaskOutcome> {
        self.outcome.as_ref()
    }

    pub fn is_terminal(&self) -> bool {
        self.state.kind().is_terminal()
    }

    pub fn begin_planning(&mut self, now_ms: u64) -> Result<TaskTransition, TaskError> {
        self.require_state(TaskStateKind::Accepted, "begin_planning")?;
        self.transition(
            TaskState::Planning,
            TaskTransitionReason::PlanningStarted,
            now_ms,
        )
    }

    pub fn add_step(&mut self, description: impl Into<String>) -> Result<usize, TaskError> {
        self.require_state(TaskStateKind::Planning, "add_step")?;
        if self.steps.len() >= self.limits.max_steps {
            return Err(TaskError::StepLimitExceeded);
        }
        let description = description.into();
        validate_text("step_description", &description, MAX_TASK_STEP_BYTES)?;
        let index = self.steps.len();
        self.steps.push(TaskStep {
            index,
            description,
            state: TaskStepState::Pending,
        });
        Ok(index)
    }

    pub fn mark_ready(&mut self, now_ms: u64) -> Result<TaskTransition, TaskError> {
        self.require_state(TaskStateKind::Planning, "mark_ready")?;
        if self.steps.is_empty() {
            return Err(TaskError::PlanHasNoSteps);
        }
        self.transition(TaskState::Ready, TaskTransitionReason::PlanReady, now_ms)
    }

    pub fn wait_for(
        &mut self,
        kind: PendingCommitmentKind,
        summary: impl Into<String>,
        ttl_ms: u64,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskError> {
        self.require_state(TaskStateKind::Ready, "wait_for")?;
        if ttl_ms == 0 || ttl_ms > self.limits.max_pending_ttl_ms {
            return Err(TaskError::InvalidPendingTtl);
        }
        let summary = summary.into();
        validate_text("pending_summary", &summary, MAX_TASK_REASON_BYTES)?;
        let expires_at_ms = now_ms.saturating_add(ttl_ms).min(self.expires_at_ms);
        let reason = match kind {
            PendingCommitmentKind::Approval => TaskTransitionReason::ApprovalPending,
            PendingCommitmentKind::Dependency => TaskTransitionReason::DependencyPending,
        };
        self.transition(
            TaskState::Waiting(PendingCommitment {
                kind,
                summary,
                expires_at_ms,
            }),
            reason,
            now_ms,
        )
    }

    pub fn resume(&mut self, now_ms: u64) -> Result<TaskTransition, TaskError> {
        if !matches!(self.state, TaskState::Waiting(_)) {
            return Err(self.invalid_transition("resume"));
        }
        if let Some(transition) = self.expire_if_needed(now_ms)? {
            return Ok(transition);
        }
        self.transition(
            TaskState::Ready,
            TaskTransitionReason::CommitmentSatisfied,
            now_ms,
        )
    }

    pub fn begin_execution(
        &mut self,
        step_index: usize,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskError> {
        self.require_state(TaskStateKind::Ready, "begin_execution")?;
        self.ensure_monotonic_time(now_ms)?;
        let step = self
            .steps
            .get_mut(step_index)
            .ok_or(TaskError::StepOutOfRange(step_index))?;
        if step.state != TaskStepState::Pending {
            return Err(TaskError::StepNotPending(step_index));
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TaskError::GenerationExhausted)?;
        step.state = TaskStepState::Executing;
        self.transition(
            TaskState::Executing {
                step_index,
                generation: self.generation,
            },
            TaskTransitionReason::StepStarted,
            now_ms,
        )
    }

    pub fn record_observation(
        &mut self,
        observed_generation: u64,
        result: ToolExecutionResult,
        verification: ToolVerificationOutcome,
    ) -> Result<(), TaskError> {
        let (step_index, expected_generation) = match self.state {
            TaskState::Executing {
                step_index,
                generation,
            } => (step_index, generation),
            _ => return Err(self.invalid_transition("record_observation")),
        };
        if observed_generation != expected_generation {
            return Err(TaskError::StaleGeneration {
                expected: expected_generation,
                observed: observed_generation,
            });
        }
        if self.observations.len() == self.limits.max_observations {
            self.observations.pop_front();
        }
        self.observations.push_back(TaskObservation {
            step_index,
            generation: observed_generation,
            result,
            verification,
        });
        Ok(())
    }

    pub fn finish_step(&mut self, now_ms: u64) -> Result<TaskTransition, TaskError> {
        let (step_index, generation) = match self.state.clone() {
            TaskState::Executing {
                step_index,
                generation,
            } => (step_index, generation),
            _ => return Err(self.invalid_transition("finish_step")),
        };
        self.ensure_monotonic_time(now_ms)?;
        self.steps[step_index].state = TaskStepState::Executed;
        if self
            .steps
            .iter()
            .all(|step| step.state == TaskStepState::Executed)
        {
            self.transition(
                TaskState::Verifying { generation },
                TaskTransitionReason::StepFinished,
                now_ms,
            )
        } else {
            self.transition(TaskState::Ready, TaskTransitionReason::StepFinished, now_ms)
        }
    }

    pub fn retry_current_step(&mut self, now_ms: u64) -> Result<TaskTransition, TaskError> {
        let step_index = match self.state {
            TaskState::Executing { step_index, .. } => step_index,
            _ => return Err(self.invalid_transition("retry_current_step")),
        };
        if self.retries >= self.limits.max_retries {
            return Err(TaskError::RetryLimitExceeded);
        }
        self.ensure_monotonic_time(now_ms)?;
        self.retries += 1;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TaskError::GenerationExhausted)?;
        self.steps[step_index].state = TaskStepState::Pending;
        self.transition(
            TaskState::Ready,
            TaskTransitionReason::RetryScheduled,
            now_ms,
        )
    }

    pub fn replan(&mut self, now_ms: u64) -> Result<TaskTransition, TaskError> {
        if !matches!(
            self.state.kind(),
            TaskStateKind::Ready | TaskStateKind::Verifying
        ) {
            return Err(self.invalid_transition("replan"));
        }
        if self.replans >= self.limits.max_replans {
            return Err(TaskError::ReplanLimitExceeded);
        }
        self.ensure_monotonic_time(now_ms)?;
        self.replans += 1;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TaskError::GenerationExhausted)?;
        self.steps.clear();
        self.observations.clear();
        self.transition(
            TaskState::Planning,
            TaskTransitionReason::ReplanStarted,
            now_ms,
        )
    }

    pub fn verify(
        &mut self,
        observed_generation: u64,
        result: VerificationResult,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskError> {
        let expected_generation = match self.state {
            TaskState::Verifying { generation } => generation,
            _ => return Err(self.invalid_transition("verify")),
        };
        if observed_generation != expected_generation {
            return Err(TaskError::StaleGeneration {
                expected: expected_generation,
                observed: observed_generation,
            });
        }
        self.ensure_monotonic_time(now_ms)?;
        let fallback_reason = || "task verification did not establish completion".to_owned();
        match result.outcome {
            TaskVerificationOutcome::Verified => {
                self.outcome = Some(TaskOutcome::Completed);
                self.transition(
                    TaskState::Completed,
                    TaskTransitionReason::VerificationPassed,
                    now_ms,
                )
            }
            TaskVerificationOutcome::Failed => {
                self.outcome = Some(TaskOutcome::Failed {
                    reason: result.reason.unwrap_or_else(fallback_reason),
                });
                self.transition(
                    TaskState::Failed,
                    TaskTransitionReason::VerificationFailed,
                    now_ms,
                )
            }
            TaskVerificationOutcome::NotVerified => {
                self.outcome = Some(TaskOutcome::NotVerified {
                    reason: result.reason.unwrap_or_else(fallback_reason),
                });
                self.transition(
                    TaskState::NotVerified,
                    TaskTransitionReason::VerificationNotProven,
                    now_ms,
                )
            }
            TaskVerificationOutcome::Cancelled => {
                self.outcome = Some(TaskOutcome::Cancelled {
                    reason: result.reason.unwrap_or_else(fallback_reason),
                });
                self.transition(
                    TaskState::Cancelled,
                    TaskTransitionReason::VerificationCancelled,
                    now_ms,
                )
            }
            TaskVerificationOutcome::Stale => {
                self.outcome = Some(TaskOutcome::NotVerified {
                    reason: result.reason.unwrap_or_else(fallback_reason),
                });
                self.transition(
                    TaskState::NotVerified,
                    TaskTransitionReason::VerificationStale,
                    now_ms,
                )
            }
        }
    }

    pub fn cancel(
        &mut self,
        reason: impl Into<String>,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskError> {
        if self.is_terminal() {
            return Err(self.invalid_transition("cancel"));
        }
        let reason = reason.into();
        validate_text("cancel_reason", &reason, MAX_TASK_REASON_BYTES)?;
        self.ensure_monotonic_time(now_ms)?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TaskError::GenerationExhausted)?;
        self.outcome = Some(TaskOutcome::Cancelled { reason });
        self.transition(
            TaskState::Cancelled,
            TaskTransitionReason::Cancelled,
            now_ms,
        )
    }

    pub fn expire_if_needed(&mut self, now_ms: u64) -> Result<Option<TaskTransition>, TaskError> {
        if self.is_terminal() {
            return Ok(None);
        }
        self.ensure_monotonic_time(now_ms)?;
        if now_ms >= self.expires_at_ms {
            self.generation = self
                .generation
                .checked_add(1)
                .ok_or(TaskError::GenerationExhausted)?;
            self.outcome = Some(TaskOutcome::Expired);
            return self
                .transition(
                    TaskState::Expired,
                    TaskTransitionReason::TaskExpired,
                    now_ms,
                )
                .map(Some);
        }
        let expired_commitment = match &self.state {
            TaskState::Waiting(commitment) if now_ms >= commitment.expires_at_ms => {
                Some(commitment.kind)
            }
            _ => None,
        };
        if let Some(kind) = expired_commitment {
            self.generation = self
                .generation
                .checked_add(1)
                .ok_or(TaskError::GenerationExhausted)?;
            let reason = match kind {
                PendingCommitmentKind::Approval => "pending approval expired",
                PendingCommitmentKind::Dependency => "pending dependency expired",
            }
            .to_owned();
            self.outcome = Some(TaskOutcome::NotVerified { reason });
            return self
                .transition(
                    TaskState::NotVerified,
                    TaskTransitionReason::CommitmentExpired,
                    now_ms,
                )
                .map(Some);
        }
        Ok(None)
    }

    fn transition(
        &mut self,
        next: TaskState,
        reason: TaskTransitionReason,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskError> {
        if now_ms < self.updated_at_ms {
            return Err(TaskError::NonMonotonicTime {
                previous: self.updated_at_ms,
                observed: now_ms,
            });
        }
        let from = self.state.kind();
        let to = next.kind();
        self.state = next;
        self.updated_at_ms = now_ms;
        Ok(TaskTransition {
            task_id: self.id.clone(),
            from,
            to,
            reason,
            at_ms: now_ms,
            generation: self.generation,
            retries: self.retries,
            replans: self.replans,
        })
    }

    fn ensure_monotonic_time(&self, now_ms: u64) -> Result<(), TaskError> {
        if now_ms < self.updated_at_ms {
            Err(TaskError::NonMonotonicTime {
                previous: self.updated_at_ms,
                observed: now_ms,
            })
        } else {
            Ok(())
        }
    }

    fn require_state(
        &self,
        expected: TaskStateKind,
        action: &'static str,
    ) -> Result<(), TaskError> {
        if self.state.kind() == expected {
            Ok(())
        } else {
            Err(self.invalid_transition(action))
        }
    }

    fn invalid_transition(&self, action: &'static str) -> TaskError {
        TaskError::InvalidTransition {
            from: self.state.kind(),
            action,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskError {
    InvalidConfiguration(&'static str),
    InvalidText {
        field: &'static str,
        limit: usize,
    },
    InvalidTransition {
        from: TaskStateKind,
        action: &'static str,
    },
    PlanHasNoSteps,
    StepLimitExceeded,
    StepOutOfRange(usize),
    StepNotPending(usize),
    RetryLimitExceeded,
    ReplanLimitExceeded,
    InvalidPendingTtl,
    StaleGeneration {
        expected: u64,
        observed: u64,
    },
    GenerationExhausted,
    NonMonotonicTime {
        previous: u64,
        observed: u64,
    },
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(field) => write!(f, "invalid task configuration: {field}"),
            Self::InvalidText { field, limit } => {
                write!(f, "{field} must be non-empty and at most {limit} bytes")
            }
            Self::InvalidTransition { from, action } => {
                write!(f, "cannot {action} from task state {from:?}")
            }
            Self::PlanHasNoSteps => f.write_str("task plan must contain at least one step"),
            Self::StepLimitExceeded => f.write_str("task step limit exceeded"),
            Self::StepOutOfRange(index) => write!(f, "task step index {index} is out of range"),
            Self::StepNotPending(index) => write!(f, "task step {index} is not pending"),
            Self::RetryLimitExceeded => f.write_str("task retry limit exceeded"),
            Self::ReplanLimitExceeded => f.write_str("task replan limit exceeded"),
            Self::InvalidPendingTtl => f.write_str("pending commitment ttl is outside task bounds"),
            Self::StaleGeneration { expected, observed } => write!(
                f,
                "stale task generation: expected {expected}, observed {observed}"
            ),
            Self::GenerationExhausted => f.write_str("task generation counter exhausted"),
            Self::NonMonotonicTime { previous, observed } => write!(
                f,
                "task logical time moved backwards: previous {previous}, observed {observed}"
            ),
        }
    }
}

impl Error for TaskError {}

impl From<EngineError> for TaskError {
    fn from(_: EngineError) -> Self {
        Self::InvalidText {
            field: "tool_observation",
            limit: crate::MAX_TOOL_OBSERVATION_BYTES,
        }
    }
}

fn validate_text(field: &'static str, value: &str, max_bytes: usize) -> Result<(), TaskError> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(TaskError::InvalidText {
            field,
            limit: max_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolResultClass;
    use serde_json::json;

    fn result(value: i64) -> ToolExecutionResult {
        ToolExecutionResult::new(
            "fixture",
            "fixture",
            "read",
            ToolResultClass::Success,
            Some(json!({ "value": value })),
            None,
            1,
        )
        .expect("bounded fixture result")
    }

    fn task() -> AgentTask {
        AgentTask::new(
            TaskId::new("task-1").expect("id"),
            "create and verify the requested artifact",
            10,
            TaskLimits::default(),
        )
        .expect("task")
    }

    #[test]
    fn successful_tool_observation_cannot_complete_task_without_verifier() {
        let mut task = task();
        task.begin_planning(11).expect("planning");
        task.add_step("read local fixture").expect("step");
        task.mark_ready(12).expect("ready");
        let execution = task.begin_execution(0, 13).expect("execute");
        task.record_observation(
            execution.generation,
            result(42),
            ToolVerificationOutcome::Verified,
        )
        .expect("observation");
        let transition = task.finish_step(14).expect("finish");
        assert_eq!(transition.to, TaskStateKind::Verifying);
        assert_eq!(task.outcome(), None);

        let generation = task.generation();
        task.verify(generation, VerificationResult::verified(), 15)
            .expect("verified task");
        assert_eq!(task.state().kind(), TaskStateKind::Completed);
        assert_eq!(task.outcome(), Some(&TaskOutcome::Completed));
    }

    #[test]
    fn stale_and_cancelled_work_cannot_resurrect_completion() {
        let mut task = task();
        task.begin_planning(11).expect("planning");
        task.add_step("step").expect("step");
        task.mark_ready(12).expect("ready");
        task.begin_execution(0, 13).expect("execute");
        task.finish_step(14).expect("verify state");
        let generation = task.generation();

        let stale = task
            .verify(
                generation.saturating_sub(1),
                VerificationResult::verified(),
                15,
            )
            .expect_err("stale generation rejected");
        assert!(matches!(stale, TaskError::StaleGeneration { .. }));
        assert_eq!(task.state().kind(), TaskStateKind::Verifying);

        task.cancel("operator cancelled", 16).expect("cancel");
        let error = task
            .verify(generation, VerificationResult::verified(), 17)
            .expect_err("cancelled task cannot complete");
        assert!(matches!(error, TaskError::InvalidTransition { .. }));
        assert_eq!(task.state().kind(), TaskStateKind::Cancelled);
    }

    #[test]
    fn pending_commitment_and_task_lifetime_expire_on_logical_time() {
        let mut task = AgentTask::new(
            TaskId::new("task-expiry").expect("id"),
            "bounded work",
            100,
            TaskLimits {
                max_lifetime_ms: 20,
                max_pending_ttl_ms: 5,
                ..TaskLimits::default()
            },
        )
        .expect("task");
        task.begin_planning(101).expect("planning");
        task.add_step("step").expect("step");
        task.mark_ready(102).expect("ready");
        task.wait_for(PendingCommitmentKind::Approval, "operator approval", 5, 103)
            .expect("wait");
        let transition = task
            .expire_if_needed(108)
            .expect("expiry")
            .expect("transition");
        assert_eq!(transition.reason, TaskTransitionReason::CommitmentExpired);
        assert_eq!(task.state().kind(), TaskStateKind::NotVerified);
    }

    #[test]
    fn observations_are_bounded_and_keep_latest_verified_evidence() {
        let mut task = AgentTask::new(
            TaskId::new("task-observations").expect("id"),
            "bounded observations",
            0,
            TaskLimits {
                max_observations: 1,
                ..TaskLimits::default()
            },
        )
        .expect("task");
        task.begin_planning(1).expect("planning");
        task.add_step("step").expect("step");
        task.mark_ready(2).expect("ready");
        let execution = task.begin_execution(0, 3).expect("execute");
        task.record_observation(
            execution.generation,
            result(1),
            ToolVerificationOutcome::NotProven,
        )
        .expect("first");
        task.record_observation(
            execution.generation,
            result(2),
            ToolVerificationOutcome::Verified,
        )
        .expect("second");
        assert_eq!(task.observations().len(), 1);
        assert_eq!(
            task.last_verified_observation()
                .expect("latest verified")
                .generation(),
            execution.generation
        );
        assert_eq!(
            task.last_verified_observation()
                .expect("latest verified")
                .result()
                .content(),
            Some(&json!({ "value": 2 }))
        );
    }

    #[test]
    fn rejected_non_monotonic_transition_is_atomic() {
        let mut task = task();
        task.begin_planning(11).expect("planning");
        task.add_step("step").expect("step");
        task.mark_ready(12).expect("ready");
        let before = task.clone();

        let error = task
            .begin_execution(0, 11)
            .expect_err("logical time regression must fail");

        assert!(matches!(error, TaskError::NonMonotonicTime { .. }));
        assert_eq!(task, before);
    }
}
