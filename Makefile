TARGET   = riscv64gc-unknown-none-elf
KERNEL_ELF = target/$(TARGET)/release/ignium-kernel
KERNEL_BIN = target/$(TARGET)/release/ignium-kernel.bin
QEMU     ?= qemu-system-riscv64
QEMUARGS  = -M virt -m 128M -nographic -bios default

build:
	cargo build --release

bin: build
	rust-objcopy -O binary $(KERNEL_ELF) $(KERNEL_BIN)

qemu: build
	$(QEMU) $(QEMUARGS) -kernel $(KERNEL_ELF)

gdb: build
	$(QEMU) $(QEMUARGS) -kernel $(KERNEL_ELF) -s -S

test: build
	@timeout 10 $(QEMU) $(QEMUARGS) -kernel $(KERNEL_ELF) 2>&1 | grep -q "M0: boot ok" \
		&& echo "TEST PASS" || (echo "TEST FAIL"; exit 1)

clean:
	cargo clean

clippy:
	cargo clippy --release -- -D warnings

fmt:
	cargo fmt --check

.PHONY: build bin qemu gdb test clean clippy fmt
