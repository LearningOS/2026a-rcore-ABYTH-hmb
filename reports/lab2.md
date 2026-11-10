# Lab2（ch4）实验报告：地址空间与 mmap / munmap

## 实验目标

ch4 把内核从"共享地址空间"升级成"每个应用一份独立地址空间"（SV39 三级页表）。
起始代码里有四个函数是空的，本实验把它们补齐：

| 函数 | 要求 |
|---|---|
| `sys_get_time` | `TimeVal` 可能横跨两个页面，不能再用裸指针直接写 |
| `sys_trace` | 读/写要查页表、校权限；非法地址与内核地址必须返回 -1 而不是把内核带崩 |
| `sys_mmap` | 参数校验 + 在用户地址空间建立映射 |
| `sys_munmap` | 参数校验 + 解除映射，物理页要真正回收 |

## 实现思路

### 1. 每任务的系统调用计数（沿用 Lab1）

ch4 的 TCB 多了 `memory_set` 等字段，但计数器的思路不变 —— **必须按任务隔离**，
否则 ch4_sleep 等并发应用的 `sys_write` 会污染 `ch4_trace1` 的断言：

```rust
// os/src/task/task.rs
pub struct TaskControlBlock {
    ...
    /// 本任务的系统调用计数，键为系统调用号
    pub syscall_counts: BTreeMap<usize, u32>,
}
```

计数仍然放在统一分发入口 `syscall()` 的第一行，这样"本次调用"本身也被计入：

```rust
pub fn syscall(syscall_id: usize, args: [usize; 3]) -> isize {
    crate::task::incr_syscall_count(syscall_id);
    match syscall_id { ... }
}
```

### 2. sys_get_time：逐页定位物理帧

`TimeVal` 是两个 `usize` 共 16 字节，若恰好压在页边界上，第二页可能根本没映射，
此时内核直接 `*ts = ...` 会在 S 态触发缺页。改为按页拆分写入：

```rust
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let us = get_time_us();
    let time_val = TimeVal { sec: us / 1_000_000, usec: us % 1_000_000 };
    let data = unsafe { core::slice::from_raw_parts(&time_val as *const _ as *const u8, 16) };
    if write_user_bytes(current_user_token(), ts as usize, data) { 0 } else { -1 }
}
```

`write_user_bytes` 每跨一页重新查一次页表，拿到 `PageTableEntry` 后用 `ppn().get_bytes_array()`
访问物理帧，因此无论数据怎么跨页都不会踩到未映射的地址。

### 3. sys_trace：查表 + 校权限

读和写都是同一个套路：`PageTable::from_token(current_user_token())` 拿到当前任务的页表，
`translate(vpn)` 得到叶子页表项，再判断 `is_valid()` 与 `readable()/writable()`：

```rust
fn read_user_byte(token: usize, addr: usize) -> isize {
    if addr >= USER_SPACE_TOP { return -1; }
    let page_table = PageTable::from_token(token);
    let va = VirtAddr::from(addr);
    match page_table.translate(va.floor()) {
        Some(pte) if pte.is_valid() && pte.readable() => {
            pte.ppn().get_bytes_array()[va.page_offset()] as isize
        }
        _ => -1,
    }
}
```

**这里有一个必须注意的坑**：`trace_read(isize::MAX)` 期望返回 `None`。
但 SV39 用 9+9+9 位索引页表，`isize::MAX`（`0x7FFF_FFFF_FFFF_FFFF`）的三级索引
全是 `0x1ff`，恰好和跳板页 `TRAMPOLINE`（`0xFFFF_FFFF_FFFF_F000`）落在**同一个叶子页表项**上。
如果不做额外判断，会误读到跳板页的代码并返回一个非 -1 的值，测试直接挂。
所以在查表前先拦住非规范地址：

```rust
/// SV39 下用户态规范虚拟地址的上界
const USER_SPACE_TOP: usize = 1 << 38;
```

至于 `0x80200000`（内核物理内存区），应用页表里根本没有对应映射，
`translate` 返回 `None`，自然得到 -1，不需要额外处理。

### 4. sys_mmap

四步校验，任何一步不过就返回 -1：

1. `len != 0`
2. `start` 页对齐（`start % PAGE_SIZE == 0`）
3. `prot` 只允许 R/W/X 三位且至少有一位（`prot != 0 && prot & !0b111 == 0`）
4. `[start, start+len)` 与当前所有 `MapArea` 都不重叠，且不越过用户地址上界

权限转换要注意：POSIX 的 `PROT_READ/WRITE/EXEC` 是 bit0/1/2，而 `MapPermission` 的
`R/W/X` 是 bit1/2/3，正好相差一位，再并上 `U` 即可：

```rust
let perm = MapPermission::from_bits(((prot as u8) << 1) | MapPermission::U.bits()).unwrap();
```

`prot = 2`（只有 W 没有 R）在 POSIX 里合法，所以 `mmap` 必须返回 0；但 RISC-V 规定
`W=1, R=0` 是**保留编码**，硬件一定会 fault。这正好对上 `ch4_mmap2` 的预期
（"只能 fault，不能正常读"），不需要做特殊处理。

映射本身直接用现成的 `insert_framed_area`（立即分配物理页，本实验不做懒分配）。
改完页表补一条 `sfence.vma`，避免 TLB 里残留旧的映射。

### 5. sys_munmap

先要求 `start` 与 `len` 都页对齐、且区间内每一页都已映射，然后在 `MemorySet` 里做真正的拆分：

```rust
pub fn unmap_range(&mut self, start_va: VirtAddr, end_va: VirtAddr) -> bool {
    // 1. 逐页确认已映射，否则整体失败
    // 2. 对每个与区间相交的 MapArea：
    //    - 先把右段 [hi, a_end) 切出来保留
    //    - 再把中段 [lo, hi) 解映射（unmap_one 会连带释放 FrameTracker）
    //    - 左段 [a_start, lo) 保留
}
```

`MapArea::split_off(from)` 负责把 `[from, end)` 切成一个新的 `MapArea`，
并把对应的物理页 `FrameTracker` 一起迁移过去，这样左右两段仍然"各自持有"自己的物理页，
被解映射的那段则在 `unmap_one` 里 drop 掉、页面真正还给分配器。

之所以不能简单地"整段删掉"：`ch4_unmap` 里先 `mmap(start, 4K)`、再 `mmap(start+4K, 8K)`，
然后 `munmap(start, 4K)` —— 只解映射第一段，后面两页必须完好无损；
反过来 `ch4_trace1` 又要求解映射后 `trace_read` 立刻返回 `None`，说明必须真的把 PTE 清掉。

## 测试结果

本地按 CI 的流程（`make test CHAPTER=4`）运行，检查项全部通过：

```
[PASS] found <Test trace OK***!>
[PASS] found <Test 04_1 OK***!>          # mmap 基本读写
[PASS] found <Test 04_4 test OK***!>     # 非法参数必须返回 -1
[PASS] found <Test 04_5 ummap OK***!>    # 解映射后可重新分配
[PASS] found <Test 04_6 ummap2 OK***!>   # 非对齐 munmap 必须失败
[PASS] found <Test trace_1 OK***!>       # trace 的地址空间校验
[PASS] not found <Test sbrk failed!>
[PASS] not found <Should cause error, Test 04_2 fail!>
[PASS] not found <Should cause error, Test 04_3 fail!>

Test passed***: 16/16
```

（`***` 是评测脚本的随机数，用来防止硬编码输出。）

## 总结

1. 进入地址空间之后，**内核不能再信任用户指针**：所有跨边界的读写都要过页表，
   这既是安全边界，也是避免 S 态缺页的必要手段。
2. `isize::MAX` 那个坑说明"按 9+9+9 索引"这种软件页表遍历和硬件的规范地址检查是两件事，
   非法地址可能"绕回"到一个合法页表项上，必须在入口显式拦住。
3. `munmap` 的真正难点是**区间拆分**：一个 `MapArea` 可能被切成两半，
   而物理页的所有权（`FrameTracker`）必须跟着走，否则要么泄漏、要么提前释放。
