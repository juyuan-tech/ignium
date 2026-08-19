//! 最小 FDT(Flattened Device Tree)解析器(M1.5)。
//!
//! 只读取内核引导所需的关键项:
//! - RAM 基址/大小(`/memory` 节点的 `reg` 属性)
//! - 定时器频率(`/cpus` 节点的 `timebase-frequency` 属性)
//! - UART 基址(兼容 `ns16550`/`ns16550a` 的串口节点)
//! - 保留内存区域(含 FDT 自身数据区)
//!
//! # 设计约束
//! - 零分配(仅栈):解析过程不分配内存,结果通过 BoardParams 返回。
//! - 大端序:FDT 始终以大端编码,按字节组装。
//! - 容错:无效指针/格式错误 → 解析静默跳过,调用方回退默认值。

use core::mem;

const FDT_MAGIC: u32 = 0xD00D_FEED;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// 已解析的板级参数。
#[derive(Clone, Copy, Default)]
pub struct BoardParams {
    pub ram_start: usize,
    pub ram_size: usize,
    pub timebase_freq: usize,
    pub uart_base: usize,
    /// 保留区列表(最多 8 项,栈分配)。
    pub reserved: [(usize, usize); 8],
    pub reserved_count: usize,
}

impl BoardParams {
    pub const fn empty() -> Self {
        BoardParams {
            ram_start: 0,
            ram_size: 0,
            timebase_freq: 0,
            uart_base: 0,
            reserved: [(0, 0); 8],
            reserved_count: 0,
        }
    }

    /// 添加保留区(去重,跳过空项)。
    fn add_reserved(&mut self, addr: usize, size: usize) {
        if addr == 0 || size == 0 {
            return;
        }
        let mut addr = addr;
        let mut size = size;
        let mut end = addr.saturating_add(size);
        // 扫描已有项,合并所有交叠的区域(可能跨越多项)。
        let mut i = 0;
        while i < self.reserved_count {
            let (r_addr, r_size) = self.reserved[i];
            let r_end = r_addr.saturating_add(r_size);
            if addr < r_end && end > r_addr {
                // 交叠:合并到当前项,移除被合并项。
                addr = addr.min(r_addr);
                end = end.max(r_end);
                size = end.saturating_sub(addr);
                // 将被合并项替换为最后一项,然后收缩计数。
                self.reserved[i] = self.reserved[self.reserved_count - 1];
                self.reserved_count -= 1;
                // 不递增 i,继续检查新换入的项。
            } else {
                i += 1;
            }
        }
        if self.reserved_count < self.reserved.len() {
            self.reserved[self.reserved_count] = (addr, size);
            self.reserved_count += 1;
        }
    }
}

/// FDT 解析器。
pub struct Fdt {
    base: *const u8,
    total_size: usize,
    off_struct: usize,
    off_strings: usize,
    off_rsvmap: usize,
    size_struct: usize,
    /// FDT 数据区自身需保留(从 fdt 指针开始,共 total_size 字节)。
    fdt_addr: usize,
}

impl Fdt {
    pub fn new(fdt: usize) -> Option<Self> {
        if fdt == 0 || !fdt.is_multiple_of(4) {
            return None;
        }
        // 保守边界:按 RISC-V Sv39 最大物理地址(2^56 - 1)校验。
        // 不能用 board::ram_end(此时 board 尚未初始化)。
        let max_phys: usize = (1usize << 56) - 1;
        if fdt
            .checked_add(mem::size_of::<FdtHeader>())
            .is_none_or(|end| end > max_phys)
        {
            return None;
        }
        let base = fdt as *const u8;
        let magic = unsafe { be32(base as usize) };
        if magic != FDT_MAGIC {
            return None;
        }
        let hdr = unsafe { &*(base as *const FdtHeader) };
        let total = be32_val(hdr.totalsize) as usize;
        let off_struct = be32_val(hdr.off_dt_struct) as usize;
        let off_strings = be32_val(hdr.off_dt_strings) as usize;
        let off_rsvmap = be32_val(hdr.off_mem_rsvmap) as usize;
        let size_struct = be32_val(hdr.size_dt_struct) as usize;
        let size_strings = be32_val(hdr.size_dt_strings) as usize;
        if total < mem::size_of::<FdtHeader>() {
            return None;
        }
        if off_struct.saturating_add(size_struct) > total
            || off_strings.saturating_add(size_strings) > total
            || off_rsvmap.checked_add(16).is_none_or(|end| end > total)
        {
            return None;
        }
        Some(Fdt {
            base,
            total_size: total,
            off_struct,
            off_strings,
            off_rsvmap,
            size_struct,
            fdt_addr: fdt,
        })
    }

    /// 解析全部参数。
    pub fn parse(&self) -> BoardParams {
        let mut params = BoardParams::empty();
        // 1) 保留 FDT 自身数据区。
        params.add_reserved(self.fdt_addr, self.total_size);
        // 2) 读取 reserve map(带边界检查,防越界读)。
        let mut off = self.off_rsvmap;
        loop {
            if off.checked_add(16).is_none_or(|end| end > self.total_size) {
                break;
            }
            let addr_hi = unsafe { be32(self.base as usize + off) };
            let addr_lo = unsafe { be32(self.base as usize + off + 4) };
            let size_hi = unsafe { be32(self.base as usize + off + 8) };
            let size_lo = unsafe { be32(self.base as usize + off + 12) };
            off += 16;
            let addr = (addr_hi as u64) << 32 | addr_lo as u64;
            let size = (size_hi as u64) << 32 | size_lo as u64;
            if addr == 0 && size == 0 {
                break;
            }
            params.add_reserved(addr as usize, size as usize);
        }
        // 3) 遍历结构块。
        let struct_ptr = self.base;
        let strings_ptr = self.base;
        let mut pos = self.off_struct;
        // H5(审计 18 轮外部):结构块边界使用 size_dt_struct 而非 total_size,
        // 防止属性值从字符串块字节中误解析。
        let struct_end = self
            .off_struct
            .saturating_add(self.size_struct)
            .min(self.total_size);
        // 当前节点名缓冲。
        let mut current_node: [u8; 64] = [0; 64];
        let mut current_node_len = 0usize;
        while pos + 4 <= struct_end {
            let token = unsafe { be32(struct_ptr as usize + pos) };
            pos += 4;
            match token {
                FDT_BEGIN_NODE => {
                    // 读取节点名(最多 63 字节,超出则跳过至 '\0')。
                    current_node_len = 0;
                    while pos < struct_end && current_node_len < 63 {
                        let c = unsafe { core::ptr::read_volatile(struct_ptr.add(pos)) };
                        if c == 0 {
                            break;
                        }
                        current_node[current_node_len] = c;
                        current_node_len += 1;
                        pos += 1;
                    }
                    current_node[current_node_len] = 0;
                    // 若节点名 >= 63 字节,跳过剩余部分直到 '\0'。
                    if current_node_len >= 63 {
                        while pos < struct_end {
                            let c = unsafe { core::ptr::read_volatile(struct_ptr.add(pos)) };
                            pos += 1;
                            if c == 0 {
                                break;
                            }
                        }
                    } else {
                        pos += 1; // 跳过 '\0'
                    }
                    pos = (pos + 3) & !3; // 对齐到 4 字节
                }
                FDT_END_NODE => {}
                FDT_PROP => {
                    if pos + 8 > struct_end {
                        break;
                    }
                    let prop_len = unsafe { be32(struct_ptr as usize + pos) } as usize;
                    let name_off = unsafe { be32(struct_ptr as usize + pos + 4) } as usize;
                    pos += 8;
                    let val_start = pos;
                    // 检查属性值是否完全在结构块内(含对齐填充)。
                    let padded = (prop_len + 3) & !3;
                    if pos.checked_add(padded).is_none_or(|end| end > struct_end) {
                        break;
                    }
                    pos += padded;
                    self.handle_prop(
                        &current_node[..current_node_len],
                        name_off,
                        strings_ptr,
                        val_start,
                        prop_len,
                        struct_ptr,
                        &mut params,
                    );
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
        params
    }

    #[allow(clippy::too_many_arguments, clippy::collapsible_match)]
    fn handle_prop(
        &self,
        node: &[u8],
        name_off: usize,
        strings_ptr: *const u8,
        val_start: usize,
        prop_len: usize,
        struct_ptr: *const u8,
        params: &mut BoardParams,
    ) {
        let name = match self.read_string(strings_ptr, name_off) {
            Some(n) => n,
            None => return,
        };
        match name {
            "reg" if node == b"memory" => {
                // 2-cell address + 2-cell size。
                if prop_len >= 16 {
                    let ah = unsafe { be32(struct_ptr as usize + val_start) };
                    let al = unsafe { be32(struct_ptr as usize + val_start + 4) };
                    let sh = unsafe { be32(struct_ptr as usize + val_start + 8) };
                    let sl = unsafe { be32(struct_ptr as usize + val_start + 12) };
                    params.ram_start = ((ah as u64) << 32 | al as u64) as usize;
                    params.ram_size = ((sh as u64) << 32 | sl as u64) as usize;
                }
            }
            "timebase-frequency" if node == b"cpus" => {
                if prop_len >= 4 {
                    params.timebase_freq =
                        unsafe { be32(struct_ptr as usize + val_start) } as usize;
                }
            }
            "compatible" => {
                if node.starts_with(b"serial@") && params.uart_base == 0 {
                    let compat =
                        unsafe { core::slice::from_raw_parts(struct_ptr.add(val_start), prop_len) };
                    let is_ns16550 = compat.windows(7).any(|w| w == b"ns16550")
                        || compat.windows(8).any(|w| w == b"ns16550a");
                    if is_ns16550 {
                        let addr_str = &node[7..];
                        if let Ok(addr) = parse_hex(addr_str) {
                            params.uart_base = addr;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn read_string<'a>(&self, strings_ptr: *const u8, offset: usize) -> Option<&'a str> {
        if offset >= self.total_size - self.off_strings {
            return None;
        }
        let max_len = self.total_size - self.off_strings - offset;
        let start = unsafe { strings_ptr.add(self.off_strings + offset) };
        let mut len = 0;
        while len < max_len {
            if unsafe { core::ptr::read_volatile(start.add(len)) } == 0 {
                break;
            }
            len += 1;
        }
        if len == 0 {
            return None;
        }
        let s = core::str::from_utf8(unsafe { core::slice::from_raw_parts(start, len) }).ok()?;
        Some(s)
    }
}

#[repr(C)]
struct FdtHeader {
    magic: [u8; 4],
    totalsize: [u8; 4],
    off_dt_struct: [u8; 4],
    off_dt_strings: [u8; 4],
    off_mem_rsvmap: [u8; 4],
    version: [u8; 4],
    last_comp_version: [u8; 4],
    boot_cpuid_phys: [u8; 4],
    size_dt_strings: [u8; 4],
    size_dt_struct: [u8; 4],
}

/// Read 4 bytes at `addr` as big-endian u32.
///
/// # Safety
/// `addr` must be readable and the 4 bytes at `addr`..`addr+4` must be valid
/// (caller verifies bounds before calling).
unsafe fn be32(addr: usize) -> u32 {
    let b = |o: usize| unsafe { core::ptr::read_volatile((addr + o) as *const u8) };
    (b(0) as u32) << 24 | (b(1) as u32) << 16 | (b(2) as u32) << 8 | b(3) as u32
}

fn be32_val(bytes: [u8; 4]) -> u32 {
    (bytes[0] as u32) << 24 | (bytes[1] as u32) << 16 | (bytes[2] as u32) << 8 | bytes[3] as u32
}

fn parse_hex(s: &[u8]) -> Result<usize, ()> {
    if s.is_empty() || s.len() > 16 {
        return Err(());
    }
    let mut val = 0usize;
    for &c in s {
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return Err(()),
        };
        val = val.checked_mul(16).ok_or(())?;
        val = val.checked_add(digit as usize).ok_or(())?;
    }
    Ok(val)
}
