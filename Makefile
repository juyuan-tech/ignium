# Ignium 构建/运行入口(开发在 WSL2 中执行;Windows 原生无 qemu/gdb 工具)。
#
# 常用命令:
#   make qemu      # 编译并启动内核到 QEMU
#   make test      # CI 同款冒烟测试(编译 + 启动 + 断言日志)
#   make gdb       # 启动 QEMU(gdb 端口 1234)+ 等待 gdb-multiarch 连接
#   make clippy    # 静态检查门禁(-D warnings)
#   make fmt       # 格式门禁
#
# 路径说明:target/ 位于 workspace 根(所有 crate 共享);链接脚本在
# kernel/linker.ld(由 .cargo/config.toml 的 rustflags 传入)。

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
#   2) 出现 "uptime:"(定时器中断 + sret 恢复链路存活,pro 审计 #8)
#   3) 未出现 "KERNEL PANIC" 或 "TRAP:"(启动后无故障)
# 说明:早期 grep -q 会在匹配后立刻 SIGPIPE 杀掉 QEMU,掩盖其后的
# 崩溃 —— 必须等 QEMU 跑满再统一断言(pro 审计 #11)。
test: build
	@timeout 10 $(QEMU) $(QEMUARGS) -kernel $(KERNEL_ELF) > /tmp/ignium-test.log 2>&1 || true
	@grep -q "M0: boot ok" /tmp/ignium-test.log \
		&& grep -q "uptime:" /tmp/ignium-test.log \
		&& ! grep -qE "KERNEL PANIC|TRAP:" /tmp/ignium-test.log \
		&& echo "TEST PASS" || (echo "TEST FAIL"; cat /tmp/ignium-test.log; exit 1)

clean:
	cargo clean

clippy:
	cargo clippy --release -- -D warnings

fmt:
	cargo fmt --check

.PHONY: build bin qemu gdb test clean clippy fmt
