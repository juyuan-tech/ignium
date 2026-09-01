# Ignium 构建/运行入口(开发在 WSL2 中执行;Windows 原生无 qemu/gdb 工具)。
#
# 常用命令:
#   make qemu      # 编译并启动内核到 QEMU
#   make test      # CI 同款冒烟测试(编译 + 启动 + 断言日志)
#   make gdb       # 启动 QEMU(gdb 端口 1234)+ 等待 gdb-multiarch 连接
#   make clippy    # 静态检查门禁(-D warnings)
#   make fmt       # 格式门禁
#
# 路径说明:target/ 位于 workspace 根(所有 crate 共享);链接脚本
# kernel/linker.ld 由 kernel/build.rs 以绝对路径传给链接器
# (CARGO_MANIFEST_DIR,不再依赖工作目录)。

TARGET   = riscv64gc-unknown-none-elf
KERNEL_ELF = target/$(TARGET)/release/ignium-kernel
KERNEL_BIN = target/$(TARGET)/release/ignium-kernel.bin
QEMU     ?= qemu-system-riscv64
# -bios default:使用 QEMU 自带 OpenSBI 固件(内核在 S 模式 @0x80200000)。
# 不要改为 -bios none:那会让 CPU 跳到 0x80000000 而非内核入口。
QEMUARGS  = -M virt -m 128M -nographic -bios default

# llvm-objcopy 由 rustup 的 llvm-tools 组件提供,位于工具链 sysroot 内,
# 不在 PATH 上 —— 自动定位(pro 审计 #9:旧写法 rust-objcopy 找不到)。
SYSROOT  := $(shell rustc --print sysroot)
HOST     := $(shell rustc -vV | sed -n 's/host: //p')
OBJCOPY  := $(SYSROOT)/lib/rustlib/$(HOST)/bin/llvm-objcopy

build:
	cargo build --release

# 生成裸二进制(烧录物理设备用;QEMU 直接用 ELF 即可)
bin: build
	$(OBJCOPY) -O binary $(KERNEL_ELF) $(KERNEL_BIN)

qemu: build
	$(QEMU) $(QEMUARGS) -kernel $(KERNEL_ELF)

# 调试:QEMU 挂起等 gdb 连接(端口 1234)
#   gdb-multiarch -ex "target remote :1234" $(KERNEL_ELF)
gdb: build
	$(QEMU) $(QEMUARGS) -kernel $(KERNEL_ELF) -s -S

# 冒烟测试:运行满 10s,收集完整输出,断言:
#   1) 出现 "M0: boot ok"(成功启动)
#   2) 出现 "buddy allocator selftest ok"(物理内存分配器自检通过)
#   2b)出现 "Sv39 paging ok"(页表 + 身份映射 + satp 切换成功)
#   2c)出现 M2 里程碑 banner(M2 T1 用户态 ecall / M2 每进程地址空间)
#   3) 出现 **至少 2 条** "uptime:"(定时器中断 + sret 恢复链路持续
#      存活;单条可能是"第一跳后定时器死亡"的假阳性,pro 审计 #6)
#   4) 未出现 "KERNEL PANIC" 或 "TRAP:"(启动后无故障)
# 说明:早期 grep -q 会在匹配后立刻 SIGPIPE 杀掉 QEMU,掩盖其后的
# 崩溃 —— 必须等 QEMU 跑满再统一断言(pro 审计 #11)。
test: build
	@timeout 10 $(QEMU) $(QEMUARGS) -kernel $(KERNEL_ELF) > /tmp/ignium-test.log 2>&1 || true
	@grep -q "M0: boot ok" /tmp/ignium-test.log \
		&& grep -q "buddy allocator selftest ok" /tmp/ignium-test.log \
		&& grep -q "Sv39 paging ok" /tmp/ignium-test.log \
		&& grep -q "kernel heap selftest ok" /tmp/ignium-test.log \
		&& grep -q "scheduler selftest ok" /tmp/ignium-test.log \
		&& grep -q "sync primitives selftest ok" /tmp/ignium-test.log \
		&& grep -q "M2 T1: user-mode thread ecall ok" /tmp/ignium-test.log \
		&& grep -q "M2: per-process address space ok" /tmp/ignium-test.log \
		&& grep -q "M2 T2a: sync IPC ok" /tmp/ignium-test.log \
		&& grep -q "M2 T2a: woken-thread preemption ok" /tmp/ignium-test.log \
		&& grep -q "M2 T2b: priority inheritance ok" /tmp/ignium-test.log \
		&& grep -q "M2 T2b: IPC stress ok" /tmp/ignium-test.log \
		&& grep -q "M2 T3a: multi-core boot ok" /tmp/ignium-test.log \
		&& grep -q "M2 T3b: per-CPU sched ok" /tmp/ignium-test.log \
		&& grep -q "M2 T3c: shared mem ok" /tmp/ignium-test.log \
		&& grep -q "M2 T3c: cap dup/revoke ok" /tmp/ignium-test.log \
		&& grep -q "M2: user fault recovery ok" /tmp/ignium-test.log \
		&& grep -q "M2: IPC latency ok" /tmp/ignium-test.log \
		&& grep -q "M3 T1: ELF loader ok (user ELF ran)" /tmp/ignium-test.log \
		&& grep -q "M3 T2: cross-core kill/shootdown ok" /tmp/ignium-test.log \
		&& grep -q "M3-2 T1: uart_server service ok" /tmp/ignium-test.log \
		&& grep -q "M3-2 T2: cross-core IPC ok" /tmp/ignium-test.log \
		&& grep -q "M3-3 T1: memory service ok" /tmp/ignium-test.log \
		&& grep -q "M3-3 T2: cross-core mem IPC ok" /tmp/ignium-test.log \
		&& grep -q "M3-4 T1: ramfs service ok" /tmp/ignium-test.log \
		&& grep -q "M3-4 T2: cross-core fs IPC ok" /tmp/ignium-test.log \
		&& test "$$(grep -c 'uptime:' /tmp/ignium-test.log)" -ge 2 \
		&& ! grep -qE "KERNEL PANIC|TRAP:" /tmp/ignium-test.log \
		&& echo "TEST PASS" || (echo "TEST FAIL"; cat /tmp/ignium-test.log; exit 1)

# 多核冒烟:boot hart 不一定是 hart 0(实测 -smp 4 时为 hart 3),
# 断言恰好 1 条 M0(引导权仲裁正确,无重复引导/无全员停车)。
# T3a:3 个副核各打印一行 "hart N online"(locked_line 整行原子,boot
# hart 等全部副核 mark_online 后才打 banner → 行不交错,断言可靠),
# 且出现 T3a banner(证明 boot hart 等到 N=4 个核全部上线)。
test-smp:
	@timeout 10 $(QEMU) $(QEMUARGS) -smp 4 -kernel $(KERNEL_ELF) > /tmp/ignium-smp.log 2>&1 || true
	@test "$$(grep -c 'M0: boot ok' /tmp/ignium-smp.log)" -eq 1 \
		&& test "$$(grep -c 'hart [0-9] online' /tmp/ignium-smp.log)" -eq 3 \
		&& grep -q "M2 T3a: multi-core boot ok" /tmp/ignium-smp.log \
		&& grep -q "M2 T3b: per-CPU sched ok" /tmp/ignium-smp.log \
		&& grep -q "M2 T3c: shared mem ok" /tmp/ignium-smp.log \
		&& grep -q "M2 T3c: cap dup/revoke ok" /tmp/ignium-smp.log \
		&& grep -q "M2: user fault recovery ok" /tmp/ignium-smp.log \
		&& grep -q "M2: IPC latency ok" /tmp/ignium-smp.log \
		&& grep -q "M3 T1: ELF loader ok (user ELF ran)" /tmp/ignium-smp.log \
		&& grep -q "M3 T2: cross-core kill/shootdown ok" /tmp/ignium-smp.log \
		&& grep -q "M3-2 T1: uart_server service ok" /tmp/ignium-smp.log \
		&& grep -q "M3-2 T2: cross-core IPC ok" /tmp/ignium-smp.log \
		&& grep -q "M3-3 T1: memory service ok" /tmp/ignium-smp.log \
		&& grep -q "M3-3 T2: cross-core mem IPC ok" /tmp/ignium-smp.log \
		&& grep -q "M3-4 T1: ramfs service ok" /tmp/ignium-smp.log \
		&& grep -q "M3-4 T2: cross-core fs IPC ok" /tmp/ignium-smp.log \
		&& ! grep -qE "KERNEL PANIC|TRAP:" /tmp/ignium-smp.log \
		&& echo "SMP TEST PASS" || (echo "SMP TEST FAIL"; cat /tmp/ignium-smp.log; exit 1)

# RVA23 P1:使用 Zba+Zbb+Zbs+Zicond 扩展编译,在 -cpu max 下验证。
# V4(自审):产物用独立 target 目录(target-rva23),不污染标准
# target/ 路径 —— 避免"make test-rva23 后默认 CPU 直跑标准 ELF
# 因 RVA23 指令而非法指令无输出"的误解(make test 会自愈重建)。
RVA23_FEATURES = -C target-feature=+zba,+zbb,+zbs,+zicond
RVA23_TARGET = target-rva23/$(TARGET)/release/ignium-kernel

build-rva23:
	CARGO_TARGET_DIR=target-rva23 cargo rustc -p ignium-kernel --release -- $(RVA23_FEATURES)

# RVA23 冒烟:与 test 相同断言,但使用 -cpu max(全扩展 CPU)。
test-rva23: build-rva23
	@timeout 10 $(QEMU) -cpu max $(QEMUARGS) -kernel $(RVA23_TARGET) > /tmp/ignium-rva23.log 2>&1 || true
	@grep -q "M0: boot ok" /tmp/ignium-rva23.log \
		&& grep -q "buddy allocator selftest ok" /tmp/ignium-rva23.log \
		&& grep -q "Sv39 paging ok" /tmp/ignium-rva23.log \
		&& grep -q "kernel heap selftest ok" /tmp/ignium-rva23.log \
		&& grep -q "scheduler selftest ok" /tmp/ignium-rva23.log \
		&& grep -q "sync primitives selftest ok" /tmp/ignium-rva23.log \
		&& grep -q "M2 T1: user-mode thread ecall ok" /tmp/ignium-rva23.log \
		&& grep -q "M2: per-process address space ok" /tmp/ignium-rva23.log \
		&& grep -q "M2 T2a: sync IPC ok" /tmp/ignium-rva23.log \
		&& grep -q "M2 T2a: woken-thread preemption ok" /tmp/ignium-rva23.log \
		&& grep -q "M2 T2b: priority inheritance ok" /tmp/ignium-rva23.log \
		&& grep -q "M2 T2b: IPC stress ok" /tmp/ignium-rva23.log \
		&& grep -q "M2 T3a: multi-core boot ok" /tmp/ignium-rva23.log \
		&& grep -q "M2 T3b: per-CPU sched ok" /tmp/ignium-rva23.log \
		&& grep -q "M2 T3c: shared mem ok" /tmp/ignium-rva23.log \
		&& grep -q "M2 T3c: cap dup/revoke ok" /tmp/ignium-rva23.log \
		&& grep -q "M2: user fault recovery ok" /tmp/ignium-rva23.log \
		&& grep -q "M2: IPC latency ok" /tmp/ignium-rva23.log \
		&& grep -q "M3 T1: ELF loader ok (user ELF ran)" /tmp/ignium-rva23.log \
		&& grep -q "M3 T2: cross-core kill/shootdown ok" /tmp/ignium-rva23.log \
		&& grep -q "M3-2 T1: uart_server service ok" /tmp/ignium-rva23.log \
		&& grep -q "M3-2 T2: cross-core IPC ok" /tmp/ignium-rva23.log \
		&& grep -q "M3-3 T1: memory service ok" /tmp/ignium-rva23.log \
		&& grep -q "M3-3 T2: cross-core mem IPC ok" /tmp/ignium-rva23.log \
		&& grep -q "M3-4 T1: ramfs service ok" /tmp/ignium-rva23.log \
		&& grep -q "M3-4 T2: cross-core fs IPC ok" /tmp/ignium-rva23.log \
		&& test "$$(grep -c 'uptime:' /tmp/ignium-rva23.log)" -ge 2 \
		&& ! grep -qE "KERNEL PANIC|TRAP:" /tmp/ignium-rva23.log \
		&& echo "RVA23 TEST PASS" || (echo "RVA23 TEST FAIL"; cat /tmp/ignium-rva23.log; exit 1)

clean:
	cargo clean
	rm -rf target-rva23

clippy:
	cargo clippy --release -- -D warnings

fmt:
	cargo fmt --check

.PHONY: build bin qemu gdb test test-smp test-rva23 clean clippy fmt
