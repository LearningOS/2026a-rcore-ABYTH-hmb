# Lab1（ch3）实验报告：实现 sys_trace

## 实验目标

在多道程序与分时多任务的内核（ch3）上实现 `sys_trace` 系统调用，使应用程序能够：

1. 读取指定用户地址的一个字节（`TraceRequest::Read`）
2. 向指定用户地址写入一个字节（`TraceRequest::Write`）
3. 查询当前任务对某个系统调用的调用次数（`TraceRequest::Syscall`）

## 实现思路

### 1. 每任务独立的系统调用计数器

测试要求"本次 sys_trace 调用本身也计入"，且 ch3 中多个应用并发运行
（如 `ch3_sleep` 会大量调用 `sys_get_time`），如果计数器是全局的，
`ch3_trace` 中 `count_syscall(SYSCALL_WRITE)` 的断言会被其他应用的
`sys_write` 干扰。因此计数器必须挂在每个任务的 TCB 上：

```rust
// os/src/task/task.rs
pub struct TaskControlBlock {
    pub task_status: TaskStatus,
    pub task_cx: TaskContext,
    /// 本任务的系统调用计数，键为系统调用号
    pub syscall_counts: BTreeMap<usize, u32>,
}
```

由于内核栈只有 8KB、任务数最多 16 个，用 `BTreeMap`（数据落堆上）
比在 TCB 里嵌大数组更安全，也避免了栈溢出的风险。

### 2. 在统一的分发入口计数

所有系统调用都经过 `os/src/syscall/mod.rs` 的 `syscall()`，在这里统一
递增计数，语义正好满足"包含当前这次调用"：

```rust
pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    crate::task::incr_syscall_count(syscall_id);
    match syscall_id { ... }
}
```

`count_syscall(SYSCALL_TRACE)` 执行时，分发入口已经把 TRACE 的计数
从 1 加到 2，断言 `assert_eq!(2, count_syscall(SYSCALL_TRACE))` 成立。

### 3. sys_trace 的三种请求

```rust
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    match trace_request {
        // Read: 读用户地址 id 处的一个字节
        0 => (unsafe { (id as *const u8).read_volatile() }) as isize,
        // Write: 向用户地址 id 写入一个字节
        1 => {
            unsafe { (id as *mut u8).write_volatile(data as u8) };
            0
        }
        // Syscall: 返回当前任务对系统调用 id 的计数
        2 => crate::task::syscall_count(id),
        _ => -1,
    }
}
```

ch3 没有引入地址空间切换，内核与应用共享同一地址空间（应用被加载到
`0x80xxxxxx` 附近），所以内核可以直接用裸指针访问用户地址；用
`read_volatile/write_volatile` 避免编译器对"看似无用"的访问做优化。

## 关键问答

**为什么 write 调用是两次？**
`println!` 每次调用会把格式化好的整段字符串通过一次 `sys_write` 写入
标准输出，测试里有两个 `println!`，所以 write 计数是 2。

**为什么计数用 BTreeMap 而不是数组？**
系统调用号不连续（64、93、124、169、410...），数组要么开得很大
（每任务 4KB，16 个任务共 64KB，超过内核栈大小），要么浪费空间；
BTreeMap 只存实际出现过的系统调用号，内存开销极小。

## 测试结果

本地运行评测脚本（与 GitHub Actions 一致的流程）：

```
[PASS] found <get_time OK! (\d+)>
[PASS] found <Test sleep OK!>
[PASS] found <current time_msec = (\d+)>
[PASS] found <time_msec = (\d+) after sleeping (\d+) ticks, delta = (\d+)ms!>
[PASS] found <Test sleep1 passed!>
[PASS] found <string from task trace test>
[PASS] found <Test trace OK!>

Test passed: 7/7
```

7 项检查全部通过。

## 总结

本实验在 ch3 内核中实现了 `sys_trace` 的三种请求。核心收获：
1. 理解了系统调用从用户态 `ecall` 到内核分发处理的完整路径；
2. 理解了为什么计数器必须按任务隔离（多道程序的并发性）；
3. 注意到内核栈/堆的尺寸限制，数据结构设计要考虑放置位置（栈 vs 堆）。
