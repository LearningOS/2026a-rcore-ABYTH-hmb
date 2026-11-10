# Lab4（ch6）实验报告：文件系统

## 实验目标

在 ch6 上接入 easy-fs 文件系统，实现三组系统调用：

| 系统调用 | 要求 |
|---|---|
| `sys_fstat` | 把文件元信息（dev / ino / mode / nlink）写回用户态 `Stat` 结构 |
| `sys_linkat` | 为已有文件建立硬链接 |
| `sys_unlinkat` | 删除目录项，硬链接归零时回收文件 |
| `sys_get_time` / `sys_mmap` / `sys_munmap` / `sys_spawn` | 从 ch4、ch5 移植（ch6 的起始代码里仍是空函数） |

## 一、easy-fs 必须改造，而不只是写系统调用

起始代码的 `easy-fs` 只有 `find` / `create` / `ls` / `read_at` / `write_at` / `clear`，
`DiskInode` 里**连硬链接计数字段都没有**：

```rust
pub struct DiskInode {
    pub size: u32,
    pub direct: [u32; 28],
    pub indirect1: u32,
    pub indirect2: u32,
    type_: DiskInodeType,
}          // ← 没有 nlink
```

而 `ch6_file1` 断言新建文件 `nlink == 1`，`ch6_file2` 断言 link 两次后 `nlink == 4`、
unlink 一次后 `== 3`。所以硬链接计数**必须落在磁盘 inode 上**（放进进程的 TCB 或
全局表都不行 —— 同一文件被不同进程 fstat 时必须看到同一个值，而且重启后不能丢）。

于是给 `DiskInode` 补上 `nlink: u32`，并在 `initialize()` 里置 1。

`DiskInode` 的大小因此从 128 字节变成 132 字节（每块由 4 个 inode 变 3 个）。
这不会破坏镜像：`efs.rs` 的布局计算全程用 `core::mem::size_of::<DiskInode>()`，
`easy-fs-fuse` 每次构建时按同一份代码重新生成 `fs.img`，两边自洽。

## 二、硬链接是"目录项 + 计数"，不是拷贝数据

`link` 做三件事：往目录里追加一个 `DirEntry`、把目标 inode 的 `nlink` 加一、
沿用原来的数据块。因此 `link(fname, lname0)` 之后，通过 `lname0` 打开读到的内容
与 `fname` 完全一致 —— `ch6_file2` 正是靠这一点验证的。

`unlink` 则是反过来：把目标目录项从目录里摘掉、`nlink` 减一、归零才真正回收 inode。

两个实现细节值得记录：

**1. 目录项删除用"末项覆盖"而不是搬移整段**

把目录里**最后一个**目录项拷到被删项的位置，再把 `size` 减一个 `DIRENT_SZ`。
这样目录始终是紧凑的，`find_inode_id` 按 `size / DIRENT_SZ` 遍历不会踩到空洞，
代价是目录项顺序会变 —— 而 easy-fs 本来就不保证顺序。

**2. `stat.ino` 必须用磁盘 inode 编号**

`ch6_file2` 断言 `stat2.ino == stat.ino`（原文件和它的硬链接）。所以 `Inode`
结构里补了 `inode_id` 字段（构造时由 `find` / `create` / `root_inode` 传入），
`fstat` 直接把它填进 `ino`。若换成"每打开一次文件分配一个编号"，这个断言立刻失败。

## 三、遇到的最大坑：批量擦除数据块会让内核静默挂死

最初 `unlink` 里按 `clear_size()` 返回的块表逐个 `dealloc_data()`，
结果 `ch6_file3`（10 轮"创建 + 写 300KB + 删除"）固定在第 9 轮**无声死亡**：
QEMU 直接退出，既没有 panic 输出，也没有内核错误信息。

定位过程（每一步都跑完整评测复现）：

| 改动 | 结果 |
|---|---|
| 跳过整个"回收数据块 + inode"段 | 31/31 通过 |
| 只 `dealloc_inode`，不碰数据块 | 31/31 通过 |
| 只 `clear_size`，不 `dealloc_data` | 31/31 通过 |
| `clear_size` + `dealloc_data` | 第 9 轮挂死 |
| 过滤非法块号后再 dealloc | 仍然挂 |
| 块缓存从 16 调到 64 | 仍然挂 |
| **`dealloc_data` 去掉"清零块内容"** | **31/31 通过** |

问题出在原始的 `dealloc_data`：

```rust
pub fn dealloc_data(&mut self, block_id: u32) {
    get_block_cache(block_id, ...)          // 把块读进缓存
        .lock()
        .modify(0, |data_block| { 全部写 0 });   // 标记为脏
    self.data_bitmap.dealloc(...);          // 释放 bitmap
}
```

清零会把**每一个**待回收的块都变成脏块，一个 150KB 的文件就有 300 多个。
块缓存只有 16 个槽位，于是这 300 个脏块在不断淘汰-回写的过程中，
和 virtio 块设备的写操作叠在一起，最终把内核拖进了无法恢复的状态
（没有任何输出，说明连 panic 的串口打印都没来得及发生）。

**修复：只释放 bitmap，不擦除块内容。**

```rust
pub fn dealloc_data(&mut self, block_id: u32) {
    self.data_bitmap.dealloc(&self.block_device,
        (block_id - self.data_area_start_block) as usize)
}
```

不擦除在语义上是安全的：被回收的块下次分配时必然挂在某个 `size = 0` 的新 inode 上，
而 easy-fs 的读写都受 `size` 约束，旧数据永远不会被上层读到。
原始代码的注释 "We will clear the block contents to zero later." 也说明擦除只是防御性动作。

## 四、其它实现要点

**`sys_fstat` 不能用裸指针写用户态**

`Stat` 是 80 字节，可能横跨两个页。若第二页未映射，裸指针写会触发内核缺页。
因此在 `mm/page_table.rs` 里加了 `write_user_bytes(token, addr, data)`：
逐页查页表、校验写权限，任何一页不可写就返回 `false`（系统调用返回 -1），
不会 panic。`sys_get_time` 写 `TimeVal` 也用同一个函数。

同时把 `USER_SPACE_TOP = 1 << 38` 拦在查表之前 —— 高于它的地址（如 `isize::MAX`）
在 SV39 下会被按低 27 位索引，"绕回"到跳板页所在的一级页表项，
不拦就会读到一个非法的"合法"值。

**`File` trait 增加 `stat()`**

`fd_table` 里存的是 `Arc<dyn File>`，要拿元信息只能在 trait 上加方法。
给 stdin/stdout 用默认实现（全零），`OSInode` 覆盖成真实值。

**`spawn` 在 ch6 要改成走文件系统**

ch5 的 `spawn` 用 `get_app_data_by_name`（应用静态打包在 `link_app.S` 里）。
ch6 已经在用文件系统，所以改成 `open_file(path, RDONLY)` 读出 ELF 再建进程。
父进程仍用 `Weak` 记录（避免 `Arc` 循环引用），并且必须把自己挂到
父进程的 `children` 里，否则 `waitpid` 找不到它。

## 测试结果

```
[PASS] found <Test file0 OK!>
[PASS] found <Test fstat OK!>
[PASS] found <Test link OK!>
[PASS] found <Test mass open/unlink OK!>
[PASS] found <file_test passed!>
... （前三个 lab 的检查项全部保留并通过）

Test passed: 31/31
Report for lab1/lab2/lab3/lab4 found.
```

## 总结

1. 文件系统的"元信息"必须落在磁盘结构上，放在内存里过不了跨进程、跨重启的一致性检查。
2. 硬链接的语义是"多个名字指向同一个 inode"，`ino` 和 `nlink` 是它的两个观测点。
3. 内核里做批量 I/O 要留意代价：300 次"读块-改块-回写"在只有 16 个槽位的块缓存上
   会退化成大量设备写，进而暴露设备层的稳定性问题。
4. 排查"静默退出"这类故障时，把问题段整段跳过 / 逐步加回（二分法）比猜测有效得多；
   而且**打过点的位置比没打点的地方先死**，本身就是定位手段。
