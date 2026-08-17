# RVA23 兼容性:现状差距与支持计划

## 1. 现状(不符合 RVA23)

| 项 | 当前 | RVA23 要求 |
|---|---|---|
| 编译目标 | `riscv64gc-unknown-none-elf` = RV64IMAFDC + Zicsr/Zifencei | RV64I + M/A/F/D/C + 一长串强制扩展 |
| QEMU virt 默认 CPU | `rv64imafdch` + `time,sstc` | —(QEMU 默认 CPU 也不完整,需 `-cpu max`) |
| 使用的扩展 | A(原子)、C、Zicsr、Sv39 | — |

**结论**:内核与工具链只覆盖 RVA23 的"RV64GC 子集",差距显著。

## 2. RVA23 强制扩展差距清单

### 已具备(无需动作)
- RV64I / M / A(Zaamo+Zalrsc,entry.S 已就地启用)/ F / D / C
- Zicsr、Zifencei、Sv39
- Zicntr(time CSR,已使用)

### 缺失(按内核价值排序)

| 扩展 | 内核用途 | 价值 |
|---|---|---|
| **Zba/Zbb/Zbs** | 位操作指令:分配器/页表的移位、位清置、前导零 | 高(代码生成改进,零成本接入) |
| **Zicond** | 条件零指令(无分支) | 中 |
| **Zicboz**(`cbo.zero`) | 页清零加速(替代 zero_page 循环) | 高(页表初始化热路径) |
| **Svpbmt** | 页级内存类型(MMIO 非缓存映射,替代 PMA 假设) | 高(真机 MMIO 正确性) |
| **Zacas** | 双字 CAS(未来能力表/同步) | 中(调度器后) |
| **Sstc** | stimecmp 硬件定时器(替代 SBI set_timer 的 ecall 开销) | 中(可选,当前 SBI 方案正确) |
| **Svinval** | 高效 TLB 失效(当前 sfence.vma 语义仍合法) | 低-中 |
| **Zicbom** | 缓存维护(DMA 一致性) | 中(DMA 出现后) |
| **V(向量)+ Zvfh 等** | **内核自身不用**(需向量上下文保存,用户态出现前无意义) | 用户态里程碑再议 |
| **H(虚拟化)** | 内核不需要 | 不纳入 |
| Sv48/Sv57 | 大内存寻址 | 真机 >128GB 再议(当前 Sv39) |

## 3. 支持计划(分阶段)

### 阶段 P1(建议 M1.5):编译目标扩展 + 验证基线
- [ ] 通过 `-C target-feature` 或自定义 target 启用 **Zba+Zbb+Zbs(+Zicond)**
      (riscv64gc 基础上追加;gc 兼容 CPU 保证向后可运行)
- [ ] CI 增加 `-cpu max` 引导矩阵(与默认 CPU 双跑,防特性回退)
- [ ] 新增 `kernel/src/cpu.rs` 启动探测:读 `misa`/平台 ISA 字符串,
      记录可用扩展(诊断 + 断言强制项)

### 阶段 P2(M2):硬件特性利用
- [ ] **Zicboz** 实现 `mem::zero_page`(无 Zicboz 时回退循环)
- [ ] **Svpbmt** 为 MMIO/页表页设置内存类型(真机正确性)
- [ ] **Zacas** 引入双字原子(能力表/无锁队列)
- [ ] **Sstc** 可选:定时器改 stimecmp(QEMU 已支持,OpenSBI 兼容层保留)

### 阶段 P3(M2+,可选):完整性
- [ ] Svinval TLB 失效接口
- [ ] Zicbom DMA 一致性
- [ ] V 向量:与 FPU 上下文保存一并设计(用户态进程切换时)
- [ ] 发布时以 RVA23 兼容平台为基线声明(文档化)

## 4. 约束与决策记录

- **内核不依赖 V/H**:保持"向量/虚拟化留给用户态/虚拟机层"的定位。
- **SBI 定时器 vs Sstc**:SBI 是固件抽象,与 RVA23 无冲突;Sstc 是
  可选优化,不阻塞合规声明。
- **QEMU 验证基线**:`-cpu max`(全扩展)与默认 CPU(子集)双跑;
  保证"带扩展编译,子集平台仍可运行"(扩展指令仅在使用处出现,
  且有运行时回退)。
- RVA23 合规的最终**外部证据**:CI 在 `-cpu max` 下跑全部测试 +
  内核启动时输出 ISA 能力表。

## 5. 登记

- ROADMAP:阶段 P1 挂 M1.5,P2 挂 M2。
- DEFERRED.md 同步条目(见 D16)。
