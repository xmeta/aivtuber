use crate::{ToolCommitStateSnapshot, ToolExecutionEvidence};
use aivtuber_domain::{
    AgentTask, PendingCommitmentKind, TaskError, TaskId, TaskLimits, TaskStateKind, TaskTransition,
    VerificationResult,
};
use aivtuber_telemetry::{TaskTransitionCollector, TaskTransitionRetentionConfig};
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;

pub const MAX_ACTIVE_TASKS_HARD: usize = 256;
pub const MAX_RETAINED_TASKS_HARD: usize = 512;
pub const MAX_TASK_TRANSITION_RECORDS_HARD: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskPlaneConfig {
    pub max_active_tasks: usize,
    pub max_retained_tasks: usize,
    pub max_transition_records: usize,
}

impl Default for TaskPlaneConfig {
    fn default() -> Self {
        Self {
            max_active_tasks: 32,
            max_retained_tasks: 64,
            max_transition_records: 1_024,
        }
    }
}

impl TaskPlaneConfig {
    fn validate(self) -> Result<Self, TaskPlaneError> {
        if self.max_active_tasks == 0 || self.max_active_tasks > MAX_ACTIVE_TASKS_HARD {
            return Err(TaskPlaneError::InvalidConfiguration("max_active_tasks"));
        }
        if self.max_retained_tasks == 0 || self.max_retained_tasks > MAX_RETAINED_TASKS_HARD {
            return Err(TaskPlaneError::InvalidConfiguration("max_retained_tasks"));
        }
        if self.max_transition_records == 0
            || self.max_transition_records > MAX_TASK_TRANSITION_RECORDS_HARD
        {
            return Err(TaskPlaneError::InvalidConfiguration(
                "max_transition_records",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub struct TaskPlane {
    active: BTreeMap<TaskId, AgentTask>,
    retained: VecDeque<AgentTask>,
    transitions: TaskTransitionCollector,
    config: TaskPlaneConfig,
}

impl Default for TaskPlane {
    fn default() -> Self {
        Self::new(TaskPlaneConfig::default()).expect("default task plane config is valid")
    }
}

impl TaskPlane {
    pub fn new(config: TaskPlaneConfig) -> Result<Self, TaskPlaneError> {
        let config = config.validate()?;
        Ok(Self {
            active: BTreeMap::new(),
            retained: VecDeque::new(),
            transitions: TaskTransitionCollector::with_retention(TaskTransitionRetentionConfig {
                max_records: config.max_transition_records,
            }),
            config,
        })
    }

    pub fn create_task(
        &mut self,
        id: TaskId,
        requested_outcome: impl Into<String>,
        created_at_ms: u64,
        limits: TaskLimits,
    ) -> Result<(), TaskPlaneError> {
        if self.task(&id).is_some() {
            return Err(TaskPlaneError::DuplicateTask(id));
        }
        if self.active.len() >= self.config.max_active_tasks {
            return Err(TaskPlaneError::ActiveCapacityExceeded);
        }
        let task = AgentTask::new(id.clone(), requested_outcome, created_at_ms, limits)?;
        self.active.insert(id, task);
        Ok(())
    }

    pub fn task(&self, id: &TaskId) -> Option<&AgentTask> {
        self.active
            .get(id)
            .or_else(|| self.retained.iter().rev().find(|task| task.id() == id))
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn retained_count(&self) -> usize {
        self.retained.len()
    }

    pub fn transitions(&self) -> &TaskTransitionCollector {
        &self.transitions
    }

    pub fn begin_planning(
        &mut self,
        id: &TaskId,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskPlaneError> {
        let transition = self.active_mut(id)?.begin_planning(now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn add_step(
        &mut self,
        id: &TaskId,
        description: impl Into<String>,
    ) -> Result<usize, TaskPlaneError> {
        Ok(self.active_mut(id)?.add_step(description)?)
    }

    pub fn mark_ready(
        &mut self,
        id: &TaskId,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskPlaneError> {
        let transition = self.active_mut(id)?.mark_ready(now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn wait_for(
        &mut self,
        id: &TaskId,
        kind: PendingCommitmentKind,
        summary: impl Into<String>,
        ttl_ms: u64,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskPlaneError> {
        let transition = self
            .active_mut(id)?
            .wait_for(kind, summary, ttl_ms, now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn resume(&mut self, id: &TaskId, now_ms: u64) -> Result<TaskTransition, TaskPlaneError> {
        let transition = self.active_mut(id)?.resume(now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn begin_execution(
        &mut self,
        id: &TaskId,
        step_index: usize,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskPlaneError> {
        let transition = self.active_mut(id)?.begin_execution(step_index, now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn record_tool_evidence(
        &mut self,
        id: &TaskId,
        observed_generation: u64,
        evidence: ToolExecutionEvidence,
    ) -> Result<(), TaskPlaneError> {
        self.active_mut(id)?.record_observation(
            observed_generation,
            evidence.result,
            evidence.verification,
        )?;
        Ok(())
    }

    pub fn finish_step(
        &mut self,
        id: &TaskId,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskPlaneError> {
        let transition = self.active_mut(id)?.finish_step(now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn retry_current_step(
        &mut self,
        id: &TaskId,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskPlaneError> {
        let transition = self.active_mut(id)?.retry_current_step(now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn replan(&mut self, id: &TaskId, now_ms: u64) -> Result<TaskTransition, TaskPlaneError> {
        let transition = self.active_mut(id)?.replan(now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn cancel(
        &mut self,
        id: &TaskId,
        reason: impl Into<String>,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskPlaneError> {
        let transition = self.active_mut(id)?.cancel(reason, now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn execution_snapshot(
        &self,
        id: &TaskId,
    ) -> Result<ToolCommitStateSnapshot, TaskPlaneError> {
        let task = self
            .task(id)
            .ok_or_else(|| TaskPlaneError::UnknownTask(id.clone()))?;
        Ok(ToolCommitStateSnapshot {
            observed_generation: task.generation(),
            cancelled: task.state().kind() == TaskStateKind::Cancelled,
        })
    }

    pub fn commit_verification(
        &mut self,
        id: &TaskId,
        expected_generation: u64,
        result: VerificationResult,
        now_ms: u64,
    ) -> Result<TaskTransition, TaskPlaneError> {
        let task = self.active_mut(id)?;
        let observed_generation = task.generation();
        if observed_generation != expected_generation {
            return Err(TaskPlaneError::StaleCommit {
                expected: expected_generation,
                observed: observed_generation,
            });
        }
        let transition = task.verify(expected_generation, result, now_ms)?;
        Ok(self.accept_transition(transition))
    }

    pub fn expire(&mut self, now_ms: u64) -> Result<usize, TaskPlaneError> {
        let ids = self.active.keys().cloned().collect::<Vec<_>>();
        let mut expired = 0;
        for id in ids {
            let transition = self.active_mut(&id)?.expire_if_needed(now_ms)?;
            if let Some(transition) = transition {
                expired += 1;
                self.accept_transition(transition);
            }
        }
        Ok(expired)
    }

    fn active_mut(&mut self, id: &TaskId) -> Result<&mut AgentTask, TaskPlaneError> {
        if self.active.contains_key(id) {
            return Ok(self.active.get_mut(id).expect("active task disappeared"));
        }
        if let Some(task) = self.retained.iter().rev().find(|task| task.id() == id) {
            return Err(TaskPlaneError::TaskNotActive {
                task_id: id.clone(),
                state: task.state().kind(),
            });
        }
        Err(TaskPlaneError::UnknownTask(id.clone()))
    }

    fn accept_transition(&mut self, transition: TaskTransition) -> TaskTransition {
        self.transitions.record(&transition);
        if transition.to.is_terminal()
            && let Some(task) = self.active.remove(&transition.task_id)
        {
            self.retained.push_back(task);
            while self.retained.len() > self.config.max_retained_tasks {
                self.retained.pop_front();
            }
        }
        transition
    }
}

#[derive(Debug)]
pub enum TaskPlaneError {
    InvalidConfiguration(&'static str),
    DuplicateTask(TaskId),
    ActiveCapacityExceeded,
    UnknownTask(TaskId),
    TaskNotActive {
        task_id: TaskId,
        state: TaskStateKind,
    },
    StaleCommit {
        expected: u64,
        observed: u64,
    },
    Task(TaskError),
}

impl fmt::Display for TaskPlaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(field) => {
                write!(f, "invalid task plane configuration: {field}")
            }
            Self::DuplicateTask(id) => write!(f, "task {id} already exists"),
            Self::ActiveCapacityExceeded => f.write_str("active task capacity exceeded"),
            Self::UnknownTask(id) => write!(f, "unknown task {id}"),
            Self::TaskNotActive { task_id, state } => {
                write!(f, "task {task_id} is no longer active ({state:?})")
            }
            Self::StaleCommit { expected, observed } => write!(
                f,
                "stale task completion commit: expected generation {expected}, observed {observed}"
            ),
            Self::Task(error) => write!(f, "{error}"),
        }
    }
}

impl Error for TaskPlaneError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Task(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TaskError> for TaskPlaneError {
    fn from(value: TaskError) -> Self {
        Self::Task(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute_verified_tool;
    use aivtuber_domain::{
        AuthenticatedControl, AuthorizationMethod, Capability, ControlSecret, EngineFuture,
        LocalControlIngress, OperatorCommandInput, TaskOutcome, ToolAction, ToolAdapter,
        ToolDescriptor, ToolExecutionResult, ToolResultClass, ToolVerificationOutcome,
        ToolVerificationRule,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};

    struct FixtureAdapter;

    impl ToolAdapter for FixtureAdapter {
        fn discover<'a>(&'a self) -> EngineFuture<'a, Vec<ToolDescriptor>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn execute<'a>(
            &'a self,
            action: &'a aivtuber_domain::AuthorizedToolAction,
        ) -> EngineFuture<'a, ToolExecutionResult> {
            Box::pin(async move {
                ToolExecutionResult::new(
                    "fixture",
                    &action.action().tool,
                    &action.action().operation,
                    ToolResultClass::Success,
                    Some(json!({ "value": 42 })),
                    None,
                    1,
                )
            })
        }
    }

    fn authenticated() -> AuthenticatedControl {
        let secret = [17_u8; 32];
        let ingress = LocalControlIngress::new(
            "task-plane-test",
            "operator:test",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([Capability::ToolGrant]),
            ControlSecret::new(secret),
        )
        .expect("trusted ingress");
        ingress
            .authenticate(
                OperatorCommandInput {
                    event_id: "evt-task".to_owned(),
                    correlation_id: "corr-task".to_owned(),
                    sequence: 1,
                    observed_at: "2026-10-05T00:00:00Z".to_owned(),
                    action: "tool.grant".to_owned(),
                    payload: BTreeMap::new(),
                },
                &secret,
            )
            .expect("authenticated")
            .authority()
            .clone()
    }

    fn id(value: &str) -> TaskId {
        TaskId::new(value).expect("task id")
    }

    fn make_ready(plane: &mut TaskPlane, task_id: &TaskId, created_at_ms: u64, limits: TaskLimits) {
        plane
            .create_task(
                task_id.clone(),
                "verified local work",
                created_at_ms,
                limits,
            )
            .expect("create");
        plane
            .begin_planning(task_id, created_at_ms + 1)
            .expect("planning");
        plane.add_step(task_id, "step one").expect("step");
        plane.mark_ready(task_id, created_at_ms + 2).expect("ready");
    }

    fn make_verifying(plane: &mut TaskPlane, task_id: &TaskId, created_at_ms: u64) -> u64 {
        make_ready(plane, task_id, created_at_ms, TaskLimits::default());
        let started = plane
            .begin_execution(task_id, 0, created_at_ms + 3)
            .expect("execute");
        plane
            .finish_step(task_id, created_at_ms + 4)
            .expect("finish");
        started.generation
    }

    #[tokio::test]
    async fn multi_step_fixture_resumes_and_completes_only_after_task_verification() {
        let mut plane = TaskPlane::default();
        let task_id = id("task-multi-step");
        plane
            .create_task(
                task_id.clone(),
                "perform two verified local steps",
                0,
                TaskLimits::default(),
            )
            .expect("create");
        plane.begin_planning(&task_id, 1).expect("planning");
        plane
            .add_step(&task_id, "read fixture one")
            .expect("step one");
        plane
            .add_step(&task_id, "read fixture two")
            .expect("step two");
        plane.mark_ready(&task_id, 2).expect("ready");

        let first = plane
            .begin_execution(&task_id, 0, 3)
            .expect("first execution");
        let evidence = execute_verified_tool(
            &FixtureAdapter,
            &authenticated(),
            ToolAction {
                tool: "fixture".to_owned(),
                operation: "read".to_owned(),
                arguments: BTreeMap::new(),
            },
            first.generation,
            || plane.execution_snapshot(&task_id).expect("snapshot"),
            &ToolVerificationRule::JsonEquals {
                pointer: "/value".to_owned(),
                expected: json!(42),
            },
        )
        .await
        .expect("tool execution");
        assert!(evidence.is_completed());
        plane
            .record_tool_evidence(&task_id, first.generation, evidence)
            .expect("record evidence");
        plane.finish_step(&task_id, 4).expect("finish first");
        assert_eq!(
            plane.task(&task_id).expect("task").state().kind(),
            TaskStateKind::Ready
        );

        plane
            .wait_for(
                &task_id,
                PendingCommitmentKind::Approval,
                "operator approval for step two",
                10,
                5,
            )
            .expect("wait");
        assert_eq!(
            plane.task(&task_id).expect("task").state().kind(),
            TaskStateKind::WaitingApproval
        );
        plane.resume(&task_id, 6).expect("resume");

        let second = plane
            .begin_execution(&task_id, 1, 7)
            .expect("second execution");
        let evidence = execute_verified_tool(
            &FixtureAdapter,
            &authenticated(),
            ToolAction {
                tool: "fixture".to_owned(),
                operation: "read".to_owned(),
                arguments: BTreeMap::new(),
            },
            second.generation,
            || plane.execution_snapshot(&task_id).expect("snapshot"),
            &ToolVerificationRule::JsonEquals {
                pointer: "/value".to_owned(),
                expected: json!(42),
            },
        )
        .await
        .expect("tool execution");
        plane
            .record_tool_evidence(&task_id, second.generation, evidence)
            .expect("record evidence");
        plane.finish_step(&task_id, 8).expect("finish second");
        assert_eq!(
            plane.task(&task_id).expect("task").state().kind(),
            TaskStateKind::Verifying
        );
        assert_eq!(plane.task(&task_id).expect("task").outcome(), None);

        plane
            .commit_verification(
                &task_id,
                second.generation,
                VerificationResult::verified(),
                9,
            )
            .expect("commit verification");
        assert_eq!(
            plane.task(&task_id).expect("retained task").outcome(),
            Some(&TaskOutcome::Completed)
        );
        assert_eq!(plane.active_count(), 0);
        assert_eq!(plane.retained_count(), 1);
    }

    #[test]
    fn failed_and_not_verified_outcomes_remain_distinguishable() {
        let mut plane = TaskPlane::default();
        let failed = id("task-failed");
        let failed_generation = make_verifying(&mut plane, &failed, 0);
        plane
            .commit_verification(
                &failed,
                failed_generation,
                VerificationResult::failed("postcondition failed").expect("result"),
                5,
            )
            .expect("failed verification");
        assert!(matches!(
            plane.task(&failed).expect("failed").outcome(),
            Some(TaskOutcome::Failed { .. })
        ));

        let unverified = id("task-unverified");
        let unverified_generation = make_verifying(&mut plane, &unverified, 10);
        plane
            .commit_verification(
                &unverified,
                unverified_generation,
                VerificationResult::not_verified("evidence missing").expect("result"),
                15,
            )
            .expect("not verified");
        assert!(matches!(
            plane.task(&unverified).expect("unverified").outcome(),
            Some(TaskOutcome::NotVerified { .. })
        ));
    }

    #[test]
    fn cancellation_and_generation_change_cannot_resurrect_completion() {
        let mut plane = TaskPlane::default();
        let cancelled = id("task-cancelled");
        let generation = make_verifying(&mut plane, &cancelled, 0);
        plane
            .cancel(&cancelled, "operator cancelled", 5)
            .expect("cancel");
        let error = plane
            .commit_verification(&cancelled, generation, VerificationResult::verified(), 6)
            .expect_err("cancelled task cannot complete");
        assert!(matches!(
            error,
            TaskPlaneError::TaskNotActive {
                state: TaskStateKind::Cancelled,
                ..
            }
        ));
        assert_eq!(
            plane.task(&cancelled).expect("cancelled").state().kind(),
            TaskStateKind::Cancelled
        );

        let stale = id("task-stale");
        let generation = make_verifying(&mut plane, &stale, 10);
        plane.replan(&stale, 15).expect("replan changes generation");
        let error = plane
            .commit_verification(&stale, generation, VerificationResult::verified(), 16)
            .expect_err("old verification cannot commit");
        assert!(matches!(error, TaskPlaneError::StaleCommit { .. }));
        assert_eq!(
            plane.task(&stale).expect("stale").state().kind(),
            TaskStateKind::Planning
        );
    }

    #[test]
    fn stale_tool_evidence_cannot_attach_to_a_new_execution_generation() {
        let mut plane = TaskPlane::default();
        let task_id = id("task-stale-evidence");
        make_ready(&mut plane, &task_id, 0, TaskLimits::default());
        let first = plane
            .begin_execution(&task_id, 0, 3)
            .expect("first execution");
        let stale_evidence = ToolExecutionEvidence {
            result: ToolExecutionResult::new(
                "fixture",
                "fixture",
                "read",
                ToolResultClass::Success,
                Some(json!({ "value": 42 })),
                None,
                1,
            )
            .expect("bounded result"),
            verification: ToolVerificationOutcome::Verified,
        };

        plane
            .retry_current_step(&task_id, 4)
            .expect("retry changes generation");
        let second = plane
            .begin_execution(&task_id, 0, 5)
            .expect("second execution");
        assert_ne!(first.generation, second.generation);

        let error = plane
            .record_tool_evidence(&task_id, first.generation, stale_evidence)
            .expect_err("stale evidence must be rejected");
        assert!(matches!(
            error,
            TaskPlaneError::Task(TaskError::StaleGeneration { .. })
        ));
        assert!(
            plane
                .task(&task_id)
                .expect("active task")
                .observations()
                .is_empty()
        );
    }

    #[test]
    fn task_capacity_retention_expiry_and_transition_history_are_bounded() {
        let mut plane = TaskPlane::new(TaskPlaneConfig {
            max_active_tasks: 1,
            max_retained_tasks: 1,
            max_transition_records: 2,
        })
        .expect("config");
        let limits = TaskLimits {
            max_lifetime_ms: 5,
            max_pending_ttl_ms: 5,
            ..TaskLimits::default()
        };
        let first = id("task-first");
        make_ready(&mut plane, &first, 0, limits);
        let capacity = plane
            .create_task(id("task-over-capacity"), "cannot fit", 1, limits)
            .expect_err("active capacity enforced");
        assert!(matches!(capacity, TaskPlaneError::ActiveCapacityExceeded));

        assert_eq!(plane.expire(5).expect("expiry"), 1);
        assert_eq!(plane.active_count(), 0);
        assert_eq!(plane.retained_count(), 1);

        let second = id("task-second");
        make_ready(&mut plane, &second, 10, limits);
        plane.cancel(&second, "done", 13).expect("cancel");
        assert!(plane.task(&first).is_none(), "old retained task must evict");
        assert_eq!(plane.retained_count(), 1);

        let metrics = plane.transitions().retention_metrics();
        assert_eq!(metrics.retained, 2);
        assert!(metrics.evicted > 0);
    }

    #[test]
    fn retry_and_replan_limits_are_enforced() {
        let limits = TaskLimits {
            max_retries: 1,
            max_replans: 1,
            ..TaskLimits::default()
        };
        let mut plane = TaskPlane::default();

        let retry = id("task-retry-limit");
        make_ready(&mut plane, &retry, 0, limits);
        plane.begin_execution(&retry, 0, 3).expect("execute");
        plane.retry_current_step(&retry, 4).expect("first retry");
        plane.begin_execution(&retry, 0, 5).expect("execute again");
        let error = plane
            .retry_current_step(&retry, 6)
            .expect_err("retry limit");
        assert!(matches!(
            error,
            TaskPlaneError::Task(TaskError::RetryLimitExceeded)
        ));

        let replan = id("task-replan-limit");
        make_ready(&mut plane, &replan, 10, limits);
        plane.replan(&replan, 13).expect("first replan");
        plane.add_step(&replan, "replacement step").expect("step");
        plane.mark_ready(&replan, 14).expect("ready");
        let error = plane.replan(&replan, 15).expect_err("replan limit");
        assert!(matches!(
            error,
            TaskPlaneError::Task(TaskError::ReplanLimitExceeded)
        ));
    }
}
