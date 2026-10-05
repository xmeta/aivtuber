use aivtuber_domain::{TaskStateKind, TaskTransition, TaskTransitionReason};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTransitionObservation {
    pub task_id: String,
    pub from: TaskStateKind,
    pub to: TaskStateKind,
    pub reason: TaskTransitionReason,
    pub at_ms: u64,
    pub generation: u64,
    pub retries: u32,
    pub replans: u32,
}

impl From<&TaskTransition> for TaskTransitionObservation {
    fn from(transition: &TaskTransition) -> Self {
        Self {
            task_id: transition.task_id.as_str().to_owned(),
            from: transition.from,
            to: transition.to,
            reason: transition.reason,
            at_ms: transition.at_ms,
            generation: transition.generation,
            retries: transition.retries,
            replans: transition.replans,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskTransitionRetentionConfig {
    pub max_records: usize,
}

impl Default for TaskTransitionRetentionConfig {
    fn default() -> Self {
        Self { max_records: 1_024 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTransitionRetentionMetrics {
    pub retained: usize,
    pub high_water: usize,
    pub evicted: u64,
}

#[derive(Debug, Clone)]
pub struct TaskTransitionCollector {
    records: VecDeque<TaskTransitionObservation>,
    retention: TaskTransitionRetentionConfig,
    high_water: usize,
    evicted: u64,
}

impl Default for TaskTransitionCollector {
    fn default() -> Self {
        Self::with_retention(TaskTransitionRetentionConfig::default())
    }
}

impl TaskTransitionCollector {
    pub fn with_retention(retention: TaskTransitionRetentionConfig) -> Self {
        assert!(
            retention.max_records > 0,
            "task transition max_records must be positive"
        );
        Self {
            records: VecDeque::new(),
            retention,
            high_water: 0,
            evicted: 0,
        }
    }

    pub fn record(&mut self, transition: &TaskTransition) {
        self.records.push_back(transition.into());
        while self.records.len() > self.retention.max_records {
            self.records.pop_front();
            self.evicted = self.evicted.saturating_add(1);
        }
        self.high_water = self.high_water.max(self.records.len());
    }

    pub fn records(&self) -> &VecDeque<TaskTransitionObservation> {
        &self.records
    }

    pub fn retention_metrics(&self) -> TaskTransitionRetentionMetrics {
        TaskTransitionRetentionMetrics {
            retained: self.records.len(),
            high_water: self.high_water,
            evicted: self.evicted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{TaskId, TaskTransition};

    fn transition(task_id: &str, generation: u64) -> TaskTransition {
        TaskTransition {
            task_id: TaskId::new(task_id).expect("task id"),
            from: TaskStateKind::Ready,
            to: TaskStateKind::Executing,
            reason: TaskTransitionReason::StepStarted,
            at_ms: generation,
            generation,
            retries: 0,
            replans: 0,
        }
    }

    #[test]
    fn task_transition_telemetry_is_bounded_and_content_free() {
        let mut collector =
            TaskTransitionCollector::with_retention(TaskTransitionRetentionConfig {
                max_records: 2,
            });
        collector.record(&transition("task-1", 1));
        collector.record(&transition("task-2", 2));
        collector.record(&transition("task-3", 3));

        let metrics = collector.retention_metrics();
        assert_eq!(metrics.retained, 2);
        assert_eq!(metrics.high_water, 2);
        assert_eq!(metrics.evicted, 1);
        assert_eq!(
            collector.records().front().expect("record").task_id,
            "task-2"
        );
        assert_eq!(collector.records().back().expect("record").generation, 3);
    }
}
