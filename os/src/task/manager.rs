//!Implementation of [`TaskManager`]
use super::TaskControlBlock;
use crate::config::BIG_STRIDE;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;

/// A stride scheduler.
///
/// 每个进程维护一个 `pass`：pass 越小说明越"欠账"，越优先被调度。
/// 被调度后 pass 增加 `BIG_STRIDE / priority`，所以优先级越高、步长越小，
/// 分到的 CPU 时间就越多。
pub struct TaskManager {
    ready_queue: Vec<Arc<TaskControlBlock>>,
}

impl TaskManager {
    ///Creat an empty TaskManager
    pub fn new() -> Self {
        Self {
            ready_queue: Vec::new(),
        }
    }

    /// Add process back to ready queue
    ///
    /// 新进程（pass 为 0）从队列里当前最小的 pass 起步，
    /// 否则它一上来就会因为 pass 远小于别人而独占 CPU。
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        let mut inner = task.inner_exclusive_access();
        if inner.pass == 0 {
            let min_pass = self
                .ready_queue
                .iter()
                .map(|t| t.inner_exclusive_access().pass)
                .min()
                .unwrap_or(0);
            inner.pass = min_pass;
        }
        drop(inner);
        self.ready_queue.push(task);
    }

    /// Take the process with the smallest `pass` out of the ready queue
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        if self.ready_queue.is_empty() {
            return None;
        }
        let mut best = 0;
        let mut best_pass = u64::MAX;
        for (i, task) in self.ready_queue.iter().enumerate() {
            let pass = task.inner_exclusive_access().pass;
            if pass < best_pass {
                best_pass = pass;
                best = i;
            }
        }
        let task = self.ready_queue.remove(best);
        {
            let mut inner = task.inner_exclusive_access();
            // 优先级越高步长越小；至少为 1，避免 pass 停滞导致独占 CPU
            let stride = (BIG_STRIDE / inner.priority as u64).max(1);
            inner.pass = inner.pass.wrapping_add(stride);
        }
        Some(task)
    }
}

lazy_static! {
    /// TASK_MANAGER instance through lazy_static!
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
}

/// Add process to ready queue
pub fn add_task(task: Arc<TaskControlBlock>) {
    //trace!("kernel: TaskManager::add_task");
    TASK_MANAGER.exclusive_access().add(task);
}

/// Take a process out of the ready queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    //trace!("kernel: TaskManager::fetch_task");
    TASK_MANAGER.exclusive_access().fetch()
}
