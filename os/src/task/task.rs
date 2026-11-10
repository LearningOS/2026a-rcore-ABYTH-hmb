//! Types related to task management

use super::TaskContext;
use alloc::collections::BTreeMap;

/// The task control block (TCB) of a task.
pub struct TaskControlBlock {
    /// The task status in it's lifecycle
    pub task_status: TaskStatus,
    /// The task context
    pub task_cx: TaskContext,
    /// The syscall counter of the task, keyed by syscall id
    pub syscall_counts: BTreeMap<usize, u32>,
}

/// The status of a task
#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    /// uninitialized
    UnInit,
    /// ready to run
    Ready,
    /// running
    Running,
    /// exited
    Exited,
}
