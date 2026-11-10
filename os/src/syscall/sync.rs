use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use crate::task::{
    block_current_and_run_next, current_process, current_task, ProcessControlBlockInner,
};
use crate::timer::{add_timer, get_time_ms};
use alloc::{sync::Arc, vec, vec::Vec};
/// sleep syscall
pub fn sys_sleep(ms: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_sleep",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let expire_ms = get_time_ms() + ms;
    let task = current_task().unwrap();
    add_timer(expire_ms, task);
    block_current_and_run_next();
    0
}
/// mutex create syscall
pub fn sys_mutex_create(blocking: bool) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mutex: Option<Arc<dyn Mutex>> = if !blocking {
        Some(Arc::new(MutexSpin::new()))
    } else {
        Some(Arc::new(MutexBlocking::new()))
    };
    let mut process_inner = process.inner_exclusive_access();
    if let Some(id) = process_inner
        .mutex_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.mutex_list[id] = mutex;
        id as isize
    } else {
        process_inner.mutex_list.push(mutex);
        process_inner.mutex_list.len() as isize - 1
    }
}
/// mutex lock syscall
pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_lock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let mut process_inner = process.inner_exclusive_access();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    if process_inner.mutex_allocation.len() <= mutex_id {
        process_inner.mutex_allocation.resize(mutex_id + 1, None);
    }
    if process_inner.mutex_request.len() <= tid {
        process_inner.mutex_request.resize(tid + 1, None);
    }
    if process_inner.deadlock_detect && mutex.is_locked() {
        // 检测：沿"持有->等待"链追踪，若绕回自己则死锁
        if mutex_deadlock(&process_inner, tid, mutex_id) {
            return -0xdead;
        }
        process_inner.mutex_request[tid] = Some(mutex_id);
        drop(process_inner);
        mutex.lock();
        let mut process_inner = process.inner_exclusive_access();
        process_inner.mutex_request[tid] = None;
        process_inner.mutex_allocation[mutex_id] = Some(tid);
    } else {
        process_inner.mutex_allocation[mutex_id] = Some(tid);
        drop(process_inner);
        mutex.lock();
    }
    0
}
/// mutex unlock syscall
pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_unlock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let mut process_inner = process.inner_exclusive_access();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    if process_inner.mutex_allocation.len() > mutex_id
        && process_inner.mutex_allocation[mutex_id] == Some(tid)
    {
        process_inner.mutex_allocation[mutex_id] = None;
    }
    drop(process_inner);
    mutex.unlock();
    0
}

/// semaphore create syscall
pub fn sys_semaphore_create(res_count: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .semaphore_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.semaphore_list[id] = Some(Arc::new(Semaphore::new(res_count)));
        process_inner.semaphore_available[id] = res_count as isize;
        id
    } else {
        process_inner
            .semaphore_list
            .push(Some(Arc::new(Semaphore::new(res_count))));
        process_inner.semaphore_available.push(res_count as isize);
        process_inner.semaphore_list.len() - 1
    };
    id as isize
}
/// semaphore up syscall
pub fn sys_semaphore_up(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_up",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let mut process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    ensure_sem_capacity(&mut process_inner, tid);
    // 调用者若持有该信号量的单元，先在账本上归还（纯 event 通知的 up 不影响账本）
    if process_inner.semaphore_allocation[tid][sem_id] > 0 {
        process_inner.semaphore_allocation[tid][sem_id] -= 1;
    }
    // 可用计数立即 +1；被唤醒线程在自己的 down 返回路径上再 -1 并接管单元
    process_inner.semaphore_available[sem_id] += 1;
    drop(process_inner);
    sem.up();
    0
}

/// semaphore down syscall
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_down",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let mut process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    ensure_sem_capacity(&mut process_inner, tid);
    if process_inner.deadlock_detect {
        // 把本次请求临时计入 need 矩阵做死锁检测；无论是否真的阻塞，
        // 记账都会在 down 返回后统一撤销/转移
        process_inner.semaphore_request[tid][sem_id] += 1;
        if semaphore_deadlock(&process_inner) {
            process_inner.semaphore_request[tid][sem_id] -= 1;
            return -0xdead;
        }
    }
    drop(process_inner);
    sem.down(); // 可能阻塞；唤醒后各账本在此处统一结算
    let mut process_inner = process.inner_exclusive_access();
    process_inner.semaphore_request[tid][sem_id] -= 1;
    process_inner.semaphore_allocation[tid][sem_id] += 1;
    process_inner.semaphore_available[sem_id] -= 1;
    0
}

/// condvar create syscall
pub fn sys_condvar_create() -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .condvar_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.condvar_list[id] = Some(Arc::new(Condvar::new()));
        id
    } else {
        process_inner
            .condvar_list
            .push(Some(Arc::new(Condvar::new())));
        process_inner.condvar_list.len() - 1
    };
    id as isize
}
/// condvar signal syscall
pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_signal",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    drop(process_inner);
    condvar.signal();
    0
}
/// condvar wait syscall
pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_wait",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    condvar.wait(mutex);
    0
}
/// enable deadlock detection syscall
///
/// 打开/关闭当前进程的死锁检测开关，返回 0 表示成功
pub fn sys_enable_deadlock_detect(enabled: usize) -> isize {
    trace!("kernel: sys_enable_deadlock_detect");
    let process = current_process();
    process.inner_exclusive_access().deadlock_detect = enabled != 0;
    0
}

/// 当前线程的 tid
fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}

/// 保证信号量分配/请求矩阵覆盖到 tid 行与全部信号量列
fn ensure_sem_capacity(inner: &mut ProcessControlBlockInner, tid: usize) {
    let n_sems = inner.semaphore_list.len();
    while inner.semaphore_allocation.len() <= tid {
        inner.semaphore_allocation.push(vec![0; n_sems]);
        inner.semaphore_request.push(vec![0; n_sems]);
    }
    let n = inner.semaphore_allocation.len();
    for row in inner.semaphore_allocation.iter_mut().take(n) {
        if row.len() < n_sems {
            row.resize(n_sems, 0);
        }
    }
    for row in inner.semaphore_request.iter_mut().take(n) {
        if row.len() < n_sems {
            row.resize(n_sems, 0);
        }
    }
    if inner.semaphore_available.len() < n_sems {
        inner.semaphore_available.resize(n_sems, 0);
    }
}

/// Coffman 死锁检测：以各信号量的真实剩余计数为 work 向量做资源分配图化简。
/// 返回 true 表示存在无法化简的线程，即死锁。
fn semaphore_deadlock(inner: &ProcessControlBlockInner) -> bool {
    let n_sems = inner.semaphore_list.len();
    let mut work: Vec<isize> = inner.semaphore_available.clone();
    let n_threads = inner.semaphore_allocation.len();
    let mut finish = vec![true; n_threads];
    for (i, row) in inner.semaphore_allocation.iter().enumerate() {
        if row.iter().any(|&x| x != 0) {
            finish[i] = false;
        }
    }
    loop {
        let mut progressed = false;
        for i in 0..n_threads {
            if !finish[i] && (0..n_sems).all(|j| inner.semaphore_request[i][j] <= work[j]) {
                for j in 0..n_sems {
                    work[j] += inner.semaphore_allocation[i][j];
                }
                finish[i] = true;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    finish.iter().any(|&f| !f)
}

/// 互斥锁死锁检测：tid 想申请 mutex_id，沿"持有者 -> 其等待的锁"链条追踪，
/// 若绕回 tid 自身则成环，返回 true。
fn mutex_deadlock(inner: &ProcessControlBlockInner, tid: usize, mutex_id: usize) -> bool {
    let mut m = mutex_id;
    let mut steps = 0;
    loop {
        steps += 1;
        if steps > inner.mutex_allocation.len() + 1 {
            return false;
        }
        match inner.mutex_allocation.get(m).copied().flatten() {
            Some(holder) if holder == tid => return true,
            Some(holder) => match inner.mutex_request.get(holder).copied().flatten() {
                Some(next) if next != m => m = next,
                _ => return false,
            },
            None => return false,
        }
    }
}
