//! Process management syscalls
use crate::config::PAGE_SIZE;
use crate::mm::{MapPermission, PageTable, VirtAddr};
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next, mmap_current_area,
    munmap_current_area, suspend_current_and_run_next, syscall_count,
};
use crate::timer::get_time_us;

/// SV39 下用户态规范虚拟地址的上界（2^38）。
///
/// 高于它的地址（如 `isize::MAX`）在硬件看来是非规范地址，
/// 但内核按 9+9+9 位索引页表时会绕回到跳板页所在的页表项，
/// 因此必须在查页表之前显式拦掉。
const USER_SPACE_TOP: usize = 1 << 38;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// get time with second and microsecond
///
/// `TimeVal` 占 16 字节，可能横跨两个页面：若第二页没有映射，
/// 直接用裸指针写会触发内核缺页。所以逐页通过页表定位物理帧后再写。
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");
    let us = get_time_us();
    let time_val = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    let data = unsafe {
        core::slice::from_raw_parts(
            &time_val as *const TimeVal as *const u8,
            core::mem::size_of::<TimeVal>(),
        )
    };
    if write_user_bytes(current_user_token(), ts as usize, data) {
        0
    } else {
        -1
    }
}

/// trace syscall
///
/// - request 0（Read）：读用户地址 id 处的一个字节，失败返回 -1
/// - request 1（Write）：把 data 的低 8 位写到用户地址 id 处，失败返回 -1
/// - request 2（Syscall）：返回当前任务对系统调用 id 的调用次数
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    let token = current_user_token();
    match trace_request {
        0 => read_user_byte(token, id),
        1 => {
            if write_user_bytes(token, id, &[data as u8]) {
                0
            } else {
                -1
            }
        }
        2 => syscall_count(id),
        _ => -1,
    }
}

/// mmap：在当前任务地址空间中申请一段用户可访问的映射
///
/// 参数与 POSIX 对齐：`prot` 的 bit0/bit1/bit2 分别表示 R/W/X。
/// 失败（非法参数、区间已被占用）返回 -1。
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");
    // 长度非 0、起点页对齐、prot 只能是 R/W/X 的组合且至少有一位
    if len == 0 || start % PAGE_SIZE != 0 || prot == 0 || prot & !0b111 != 0 {
        return -1;
    }
    let end = match start.checked_add(len) {
        Some(end) if end <= USER_SPACE_TOP => end,
        _ => return -1,
    };
    // POSIX 的 PROT_READ/WRITE/EXEC 恰好对应 MapPermission 的 R/W/X 位（都从 bit1 起）
    let perm = match MapPermission::from_bits(((prot as u8) << 1) | MapPermission::U.bits()) {
        Some(perm) => perm,
        None => return -1,
    };
    if mmap_current_area(VirtAddr::from(start), VirtAddr::from(end), perm) {
        flush_tlb();
        0
    } else {
        -1
    }
}

/// munmap：解除当前任务地址空间中一段已建立的映射
///
/// 要求 start 与 len 都页对齐，且区间内每一页都已映射，否则返回 -1。
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    if len == 0 || start % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 {
        return -1;
    }
    let end = match start.checked_add(len) {
        Some(end) if end <= USER_SPACE_TOP => end,
        _ => return -1,
    };
    if munmap_current_area(VirtAddr::from(start), VirtAddr::from(end)) {
        flush_tlb();
        0
    } else {
        -1
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// 修改页表后刷新 TLB，避免残留的旧映射被继续使用
fn flush_tlb() {
    unsafe { core::arch::asm!("sfence.vma") };
}

/// 读取用户地址 `addr` 处的一个字节：必须已映射且有读权限，否则返回 -1
fn read_user_byte(token: usize, addr: usize) -> isize {
    if addr >= USER_SPACE_TOP {
        return -1;
    }
    let page_table = PageTable::from_token(token);
    let va = VirtAddr::from(addr);
    match page_table.translate(va.floor()) {
        Some(pte) if pte.is_valid() && pte.readable() => {
            pte.ppn().get_bytes_array()[va.page_offset()] as isize
        }
        _ => -1,
    }
}

/// 把 `data` 逐字节写到用户地址 `addr`，每跨一页重新查一次页表。
/// 任一页未映射或没有写权限则返回 false。
fn write_user_bytes(token: usize, addr: usize, data: &[u8]) -> bool {
    if addr.checked_add(data.len()).is_none() || addr + data.len() > USER_SPACE_TOP {
        return false;
    }
    let mut done = 0usize;
    while done < data.len() {
        let va = VirtAddr::from(addr + done);
        let pte = match PageTable::from_token(token).translate(va.floor()) {
            Some(pte) if pte.is_valid() && pte.writable() => pte,
            _ => return false,
        };
        let offset = va.page_offset();
        let n = core::cmp::min(PAGE_SIZE - offset, data.len() - done);
        pte.ppn().get_bytes_array()[offset..offset + n].copy_from_slice(&data[done..done + n]);
        done += n;
    }
    true
}
