# Lab5（ch8）实验报告：并发与死锁检测

## 实验目标

在 ch8（并发与同步）上完成两类工作：

| 系统调用 | 要求 |
|---|---|
| `sys_enable_deadlock_detect` | 打开/关闭当前进程的死锁检测开关 |
| `sys_mutex_lock` / `sys_semaphore_down` | 开关打开时，阻塞请求若会导致死锁，返回 `-0xdead` 而不真正阻塞 |
| `sys_get_time` / `sys_mmap` / `sys_munmap` / `sys_spawn` / `sys_set_priority` / `sys_fstat` / `sys_linkat` / `sys_unlinkat` | 从 ch4、ch5、ch6 移植（ch8 的起始代码里仍是空函数） |

起始代码已经给全了自旋锁/阻塞互斥锁、信号量、条件变量的实现（`sync/mutex.rs`、
`sync/semaphore.rs`、`sync/condvar.rs`）以及对应的系统调用分发，本章的正题是
**死锁检测**。

## 一、死锁检测的建模

三个死锁测试用例决定了算法形态：

- `ch8_deadlock_mutex1`：单线程对**同一个阻塞互斥锁**连续 lock 两次，第二次必须
  返回 `-0xdead`；
- `ch8_deadlock_sem1`：3 个线程各持有不同数量的信号量资源（1/2/1），再交叉申请
  对方持有的资源，构成真正的循环等待，**至少一个线程**要拿到 `-0xdead`；
- `ch8_deadlock_sem2`：4 个线程、2 类资源各 2 个，需求矩阵恰好可满足，**一个都不许
  误报**（`failed == 0`）。

测试还共用一个"屏障"信号量：主线程先 `down` 3（或 4）次把它清零，子线程再各自
`down` 一次阻塞在它上面，等主线程 `sleep` 后 `up` 放行。这个模式说明检测算法必须
**容忍"暂时无人能前进但之后会被释放"的状态**。

因此采用教材上的 **Coffman 死锁检测算法**（资源分配图化简）：

1. 维护三个账本（记在 `ProcessControlBlockInner` 里）：
   - `semaphore_allocation[tid][sem]`：线程持有的信号量单元数；
   - `semaphore_request[tid][sem]`：线程**正在阻塞等待**的单元数；
   - 互斥锁：`mutex_allocation[mutex_id] -> tid`（持有者）、
     `mutex_request[tid] -> mutex_id`（等待中的锁）。
2. 每次请求发生时，把这笔请求临时计入 `request`，然后化简：
   - work 向量 = 各信号量的**可用计数镜像**（`available`：create 时置为初始
     计数，`up` 立即 +1，`down` 的 −1 记在返回之后）；
   - 一个线程"可完成"当且仅当它的全部请求 ≤ work；完成即归还它持有的全部资源；
   - 反复化简直到无线程可完成；仍有线程未完成 ⇒ 死锁 ⇒ 返回 `-0xdead` 并撤销
     这次临时记账。
3. 互斥锁是单单元资源，用"持有→等待"链追踪即可：申请 `m` 时若 `m` 已被持有，
   沿持有者的等待边一路走下去，绕回申请者自身即成环（加步数上限防兜圈）。

关键点一：**屏障场景为什么不误报**。主线程先 `down` 屏障 3（或 4）次恰好把计数
清零——主线程的 allocation 是 3（或 4）。子线程阻塞在屏障上时做化简：主线程没有
未满足请求，视为可完成，把它持有的屏障单元还给 work，于是所有子线程的屏障请求
都能被满足 ⇒ 不判死锁（正确，主线程随后会放行）。

关键点二：**work 向量必须用 available 镜像，不能用信号量的真实计数**。这是实测
踩过的坑：`ch8_deadlock_sem2` 里主线程 4 次 `up` 屏障后，被唤醒的子线程还没来得及
恢复执行做记账，此时屏障的**真实计数是 0**，但那 4 个单元其实已经承诺给被唤醒的
线程了。用真实计数当 work 会把"等待中的屏障请求"判成永远无法满足，误报死锁。
镜像的正确性在于：`up` 立即 +1（单元已移交），`down` 的 −1 记在该线程从
`down` 返回时，正好刻画"已承诺未接管"的中间态。

## 二、账本的一致性

账本全部在 `sys_mutex_lock/unlock`、`sys_semaphore_down/up` 的系统调用层维护：

- `down` 的记账与是否阻塞**无关、统一在 `down` 返回后结算**：request 清零、
  allocation 加一、available 减一。阻塞的线程在阻塞期间 request 一直挂着，
  恰好构成检测需要的"未满足请求"；不阻塞的线程记完就清，净效果为零；
- `up` 时若调用者持有该信号量的单元则归还 allocation（纯 event 通知式的 `up`
  不动账本，避免把"没持有也 up"的用法算坏），并让 available 立即 +1——被唤醒
  线程在自己的 `down` 返回路径上再 −1 并接管单元，账本自然完成移交；
- 互斥锁给 `Mutex` trait 加了 `is_locked()`（自旋锁读 `locked`，阻塞锁读
  `inner.locked`），用真实锁状态决定"直接授予"还是"进入检测/阻塞"路径，避免
  账本与真实状态漂移（例如 `Condvar::wait` 内部直接调用 `mutex.unlock/lock`
  绕过系统调用层的情况）。

`sys_enable_deadlock_detect` 只做一件事：设置 `deadlock_detect` 开关。账本的维护
**与开关无关、始终进行**，这样开关打开时矩阵总是新鲜的；而"是否做环路检测"才由
开关控制。

## 三、前几章系统调用的移植

ch8 起始代码里 ch4/ch5/ch6 的作业都是空函数，按既有实现移植：

- `sys_get_time`：`TimeVal` 16 字节可能跨页，用 `write_user_bytes` 逐页写；
- `sys_mmap`/`sys_munmap`：参数校验（长度非 0、页对齐、prot ∈ R|W|X 非空）→
  `is_range_free` / `unmap_range`；`PROT_xxx` 与 `MapPermission` 的 R/W/X 位差 1，
  `(prot << 1) | U` 直接换算；`sfence.vma` 刷 TLB；
- `sys_spawn`：`ProcessControlBlock::new(elf)` 直接建子进程（不等价 fork+exec），
  父指针用 `Arc::downgrade`，并必须挂进父进程 `children`，否则 `waitpid` 找不到；
- `sys_fstat`/`sys_linkat`/`sys_unlinkat`：依赖 easy-fs 的 `nlink`/`inode_id`
  （lab4 的改造原样搬过来）；`Stat` 可能跨页，同样逐页写。

与 ch5/ch6 不同的一点：ch8 里 `memory_set` 挂在 `ProcessControlBlock` 而非 TCB 上，
所以 `mmap_current_area` / `munmap_current_area` 两个辅助函数改为对 `current_process()`
的地址空间操作。

## 四、验证结果

```
$ ~/rcore-test-dev.sh 8
Test passed: 31/31
```

覆盖 `ch8_usertest` 的全部 23 项：3 个死锁检测（mutex 自锁、多线程信号量循环等待、
无死锁不误报）、哲学家就餐、生产者-消费者（mpsc + 信号量）、条件变量、自旋锁竞态
计数器、多线程与传参、ch2~ch7 的回归用例等，外加 reports/lab1~lab5 检查。
