# Lab3（ch5）实验报告：进程管理与 stride 调度

## 实验目标

ch5 把内核从"一个任务一个地址空间"推进到真正的**进程**模型：进程有父子关系、
可以 `fork` / `exec` / `waitpid`，内核里还多了一个特殊的 `initproc` 负责收留孤儿进程。
起始代码已经给出了 `fork` / `exec` / `waitpid` / `sbrk`，本实验要补的是五处：

| 函数 | 要求 |
|---|---|
| `sys_get_time` | 同 Lab2，`TimeVal` 可能跨页 |
| `sys_mmap` / `sys_munmap` | 同 Lab2，加在当前进程的地址空间上 |
| `sys_spawn` | 用指定 ELF 直接创建一个全新子进程，返回 pid |
| `sys_set_priority` | 设置当前进程优先级，`prio < 2` 返回 -1 |

另外起始代码的调度器是纯 FIFO 的 `VecDeque`，需要改造成 **stride 调度**，
`set_priority` 才有实际意义。

## 实现思路

### 1. sys_spawn：不是 fork + exec

这两者的区别是本章最核心的一句话：

- `fork` 先把父进程的**整份地址空间复制一份**，子进程从 `fork` 返回处继续跑，
  要再调一次 `exec` 才能换成别的程序 —— 中间那次"复制再丢弃"纯属浪费。
- `spawn` 直接用目标 ELF 从零构造地址空间，子进程的入口就是 ELF 的 `entry point`。

```rust
pub fn sys_spawn(path: *const u8) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(data) = get_app_data_by_name(path.as_str()) {
        let parent = current_task().unwrap();
        let child = TaskControlBlock::new(data);   // 直接按 ELF 建新进程
        let pid = child.getpid();
        child.inner_exclusive_access().parent = Some(Arc::downgrade(&parent));
        let child = Arc::new(child);
        parent.inner_exclusive_access().children.push(child.clone());
        add_task(child);
        pid as isize
    } else {
        -1
    }
}
```

两个容易漏的点：

1. **必须挂到父进程的 `children` 里**，否则 `waitpid` 找不到这个子进程，
   `ch5_spawn0` 里的 `wait()` 会一直返回 -2。
2. **`parent` 要用 `Weak`**，否则父进程和子进程互相 `Arc` 会形成循环引用，
   进程退出后地址空间永远不释放。

### 2. sys_set_priority 与 stride 调度

优先级只在"能被调度器感知"时才有意义，所以同时改了调度器。

**数据结构**：`TaskControlBlockInner` 增加两个字段

```rust
pub priority: usize,   // 默认 16
pub pass: u64,         // stride 调度里的"已欠账"
```

**调度算法**：每次从就绪队列里挑 `pass` 最小的进程运行，运行后

```rust
let stride = (BIG_STRIDE / inner.priority as u64).max(1);
inner.pass = inner.pass.wrapping_add(stride);
```

`BIG_STRIDE = 1 << 32`。优先级越高 → `stride` 越小 → `pass` 增长越慢 →
被选中的次数越多，于是 CPU 时间**按优先级成比例分配**。

两个细节：

- `.max(1)` 不能省。当 `priority = isize::MAX` 时 `BIG_STRIDE / prio = 0`，
  `pass` 永远不增长，这个进程就会一直占着 CPU 不放。取 `max(1)` 让它退化成"最快"而不是"永占"。
- **新进程的 `pass` 要初始化为队列里的最小值**，而不是 0：

```rust
if inner.pass == 0 {
    let min_pass = self.ready_queue.iter()
        .map(|t| t.inner_exclusive_access().pass).min().unwrap_or(0);
    inner.pass = min_pass;
}
```

否则新进程 `pass = 0` 远小于已经在跑的进程，一加入队列就会长时间独占 CPU，
`ch5_spawn0` 那种"一口气 spawn 40 个"的场景会明显卡顿。

**系统调用本身**很简单：

```rust
pub fn sys_set_priority(prio: isize) -> isize {
    if prio < 2 { return -1; }
    current_task().unwrap().inner_exclusive_access().priority = prio as usize;
    prio
}
```

注意返回值是**设置后的优先级**而不是旧值 —— `ch5_setprio` 依次断言
`set_priority(10) == 10`、`set_priority(isize::MAX) == isize::MAX`，正是这个语义。

### 3. get_time / mmap / munmap

这三个和 Lab2 完全同构，只是"当前任务的地址空间"从 `TASK_MANAGER` 换成
`current_task().inner_exclusive_access().memory_set`：

- `sys_get_time` 仍然逐页查表写，避免 `TimeVal` 跨页时触发内核缺页；
- `sys_mmap` 校验 `len != 0`、页对齐、`prot ∈ 1..=7`、区间不与已有 `MapArea` 重叠，
  权限位用 `MapPermission::from_bits((prot << 1) | U.bits())` 转换；
- `sys_munmap` 要求页对齐 + 整段已映射，`MemorySet::unmap_range` 负责把命中的
  `MapArea` 切成左右两段保留，被解映射部分的 `FrameTracker` 随之释放。

此外 ch5 的 `mm/mod.rs` 把 `PageTable` 私有导入了（`use` 而非 `pub use`），
导致 syscall 层拿不到；改成 `pub use` 才能写页表级的用户内存访问。

## 测试结果

```
[PASS] found <get_time OK***! (\d+)>
[PASS] found <Test sleep OK***!>
[PASS] found <current time_msec = (\d+)>
[PASS] found <time_msec = (\d+) after sleeping (\d+) ticks, delta = (\d+)ms!>
[PASS] found <Test sleep1 passed***!>
[PASS] found <Test 04_1 OK***!>
[PASS] found <Test 04_4 test OK***!>
[PASS] found <Test 04_5 ummap OK***!>
[PASS] found <Test 04_6 ummap2 OK***!>
[PASS] found <Test spawn0 OK***!>
[PASS] found <Test wait OK***!>
[PASS] found <Test waitpid OK***!>
[PASS] found <Test set_priority OK***!>
[PASS] not found <FAIL: T.T>
[PASS] not found <Test sbrk failed!>

Test passed***: 15/15
```

### stride 调度的实测验证

`ch5_stride` 让 6 个子进程各自以不同优先级空转 4 秒，统计循环次数：

| 优先级 | count | count / priority |
|---|---|---|
| 5 | 5,557,200 | 1,111,440 |
| 6 | 6,970,400 | 1,161,733 |
| 7 | 7,996,400 | 1,142,342 |
| 8 | 9,259,200 | 1,157,400 |
| 9 | 10,050,800 | 1,116,755 |
| 10 | 11,796,400 | 1,179,640 |

`count / priority` 稳定在 1.11M 到 1.18M（±5%），说明各进程拿到的 CPU 时间
确实与优先级成正比，调度器符合预期。

## 总结

1. `spawn` 与 `fork + exec` 的差别不只是性能，更是**语义**：spawn 出来的子进程
   不需要继承父进程的执行现场，因此从 ELF 直接构造地址空间是最自然的做法。
2. stride 调度把"优先级"这个抽象概念落成了一个可验证的数值关系
   （`count ∝ priority`），比 FIFO 加时间片更容易解释和测试。
3. 调度器里的边界条件（`pass` 初值、`stride` 为 0）在平均负载下看不出来，
   但在"一次 spawn 40 个进程"这种极端场景下会直接暴露，这说明并发代码
   必须在极端输入下验证。
