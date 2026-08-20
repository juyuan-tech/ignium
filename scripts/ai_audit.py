#!/usr/bin/env python3
"""Independent AI code audit for Ignium (炬元微内核).

Collects all kernel sources and asks an external LLM (DeepSeek,
OpenAI-compatible API) for an independent security/bug review.

Usage:
    IGNIUM_AUDIT_KEY=sk-xxx python3 scripts/ai_audit.py [--key-file PATH]
    IGNIUM_AUDIT_MODEL=deepseek-v4-pro   # default; switch models via env
    IGNIUM_AUDIT_URL=...                 # override API endpoint

The API key is read from the environment or a one-shot local file.
It is never written into the repository and never sent to the model.
"""

import argparse
import datetime
import glob
import json
import os
import sys
import urllib.error
import urllib.request

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
API_URL = os.environ.get("IGNIUM_AUDIT_URL", "https://api.deepseek.com/chat/completions")
MODEL = os.environ.get("IGNIUM_AUDIT_MODEL", "deepseek-v4-pro")
EFFORT = os.environ.get("IGNIUM_AUDIT_EFFORT", "max")
TIMEOUT = 1800

GLOBS = [
    "kernel/src/**/*.rs",
    "kernel/src/**/*.S",
    "kernel/build.rs",
    "kernel/Cargo.toml",
    "kernel/linker.ld",
    "Cargo.toml",
    "Makefile",
    "rust-toolchain.toml",
    ".cargo/config.toml",
    ".github/workflows/*.yml",
    "README.md",
    "docs/DESIGN.md",
    "docs/DEFERRED.md",
    "ROADMAP.md",
]

INSTRUCTIONS = """\
You are an independent external reviewer for a from-scratch operating system
kernel. You are NOT the author. Assume the author's self-review is biased and
incomplete. Your job is to find what the author missed. Be thorough and
exhaustive — do not stop at surface-level issues. Flag everything suspicious.

IMPORTANT: This is audit V4. The previous audits (V2 20260819-122344, V3
20260819-135719) found many issues; ALL have been fixed and committed. Since
V3, the author also completed D17 (SSTC detection + SBI timer fallback) and
captured an M1.5 benchmark baseline (docs/benchmarks.md). Do NOT re-report
previously fixed issues. Focus on finding NEW issues the prior audits and the
author's self-reviews missed.

Previously fixed issues (do not re-report):
- Buddy/C1: only first 16MiB free-listed with reserved regions → fixed (loop all orders)
- frame_restore/C2: lost outgoing context → fixed (saves old_ctx before sret)
- Stack guard/H1: silent unmap failure on 2MiB boundary → assert + fail-loudly
- unmap_4k/H2: no TLB flush → sfence.vma inside; map_4k documented for post-boot remap
- pick_next/H3: fallback could return Blocked → state != Running → idle
- SBI/H4: set_timer t0-t6 clobbers → clobber_abi("C") (also Safety doc)
- FDT/H5: struct_end used total_size → bound by size_dt_struct
- heap/H6: slab dealloc no slot-alignment check → offset/is_multiple/range check
- Thread stacks no guard → registered (DEFERRED D20, M2)
- RAM alignment & coverage → 2MiB + covers kernel + Sv39 window + start floor >= 0x80000000
- FDT: 8-byte alignment, node-name stack, read_string lifetime, reserved multi-interval
- Timer: deadline catch-up (no storm), now+interval wrapping, interval read cached (1x)
- Scheduler: woken cleared in pick_next, stress tests (16 threads), priority preemption
- FDT robustness: totalsize bound <=16MiB, fdt+total<=max_phys, depth>8 stops matching,
  UART base from node name OR reg property, memory 1-cell + 2-cell address support
- mmu: OpenSBI region [ram_start, kernel_start) read-only R (was RW, write faults loudly)
- UART init: mmio_fence between DLL/DLM and FCR/MCR writes (real-HW baud divisor)
- D17: USE_SSTC flag + arm_timer dual path; enable_timer uses SBI first (no illegal-instr
  trap on non-SSTC); cpu.rs detects 'sstc' in riscv,isa and switches to stimecmp
- Panic diagnostic for thread stacks; alloc_zeroed_free_stack renamed alloc_free_stack
- Scheduler test relabeled (busy-thread exit vs preemption intent); CI adds SMP + rva23 clippy/fmt

Project context:
- Language: Rust (no_std + alloc), edition 2021, toolchain pinned 1.97.1
- Architecture: RISC-V 64 (riscv64gc), running in S-mode on QEMU virt
  machine (OpenSBI 1.3 firmware), kernel linked at 0x80200000
- Stage: **M1.5 complete** (M1: trap/timer/buddy/Sv39 paging/kernel heap/
  cooperative+preemptive scheduler/Mutex+Condvar/SpinLock IRQ-safe.
  M1.5: FDT parser + board params runtime / page permission split (code RX,
  rodata R, data RW, heap RW no-X; OpenSBI region R) / stack guard pages /
  RVA23 P1 (Zba+Zbb+Zbs+Zicond, -cpu max CI) / pressure stress tests /
  page table interface (unmap_4k, tlb_flush) / D17 SSTC+SBI timer dual path /
  benchmark baseline in docs/benchmarks.md).
- Timer: D17 dual path — first deadline via SBI, after FDT ISA parse switches to
  stimecmp if 'sstc' present (USE_SSTC flag). timer-driven preemption via full
  trap-frame swap in the ISR (ctx_valid/frame_valid dual resume protocol).
  Cooperative yield via callee-saved context switch.
  Scheduler: 2-level priority (HIGH/LOW), tick-based preemption (10 ticks = 100ms),
  idle thread, exit/reaper, lost-wake protection (woken flag).
- IRQ-safe SpinLock (saves/restores SIE on lock/unlock); Mutex (block/wake
  with waiters queue), Condvar (wait/notify_one/notify_all).
- Buddy allocator (order 0..12, 4KB pages, 128 MiB max, MAX_PAGES=32768,
  carve for FDT reserved regions, multi-interval support). Slab heap (8
  classes 16B..2KB, page chain traversal, page path for large objects).
  HEAP dealloc uses cached allocator byte count (no lock on hot path).
- FDT parser (kernel/src/fdt.rs): minimal, extracts RAM range, timebase
  frequency, UART base (node name or reg), reserved regions, ISA string.
  8-byte alignment, node-name stack, struct_end bounded by size_dt_struct,
  totalsize <=16MiB. Board params (board.rs): runtime functions with FDT
  fallback and QEMU virt defaults; validated (coverage, alignment, Sv39, floor).
- Sv39 identity mapping with per-section permissions (D2), stack guard
  pages (D4, 4KB unmapped below boot/trap stacks, assert unmap success).
- CPU capability detection (cpu.rs): ISA string from FDT (RVA23 P1); drives
  D17 SSTC choice. RVA23 CI: separate job with -cpu max, Zba+Zbb+Zbs+Zicond.
- Single-core only (secondary harts parked in entry.S via BOOT_LOCK
  arbitration). Multi-core planned for M2 (D7 per-hart trap stacks, D8
  secondary hart wake, D9 console lock, D19 multi-core scheduler).
- Perf baseline: slab 64B alloc+dealloc ~19-24ns/op; context switch ~200-284ns/op
  (yield path), measured on QEMU (see docs/benchmarks.md).
- Future: microkernel (user processes/IPC/capabilities) with
  OpenHarmony-compatible userspace layer. M2: user processes, IPC,
  capabilities, multi-core bring-up.

Review requirements — examine from ALL of the following perspectives:
1. Memory safety: UB, unsafe misuse, volatile access, pointer arithmetic,
   alignment, integer overflow, dangling references, use-after-free,
   buffer overflow in static arrays (MAX_PAGES, SLAB_PAGE_CLASS, etc.).
2. Control flow & state machine: boot sequence, panic paths, infinite loops,
   unreachable states, catastrophic hang scenarios, scheduler thread state
   machine (Ready/Running/Blocked/Exited transitions, ctx_valid/frame_valid
   dual protocol correctness).
3. Concurrency & reentrancy: atomic ordering (Acquire/Release correctness),
   ISR-safety of log paths (zero-logging constraint), double-fault
   recursion, lock ordering (WAITERS->SCHED->HEAP acyclic?), lost-wake
   scenarios (woken flag), interrupt context hazards, nested irq_save/
   restore correctness, SpinLock irq_save/restore pairing.
4. Threat model: hostile bootloader, corrupted RAM, register/CSR misuse,
   information leakage, privilege issues, attacker-influenced values,
   FDT parser robustness against malformed/corrupted input (bounds checks,
   UTF-8 validation, integer overflow).
5. Build & reproducibility: linker script correctness (section layout,
   _alloc_start, guard pages), orphan sections, toolchain drift, CI
   pitfalls, debug-vs-release divergence, RVA23 extension compatibility.
6. Hidden bugs: things that work in QEMU but would break on real hardware
   (timing assumptions, cache behavior, MMIO fence correctness, PMP
   restrictions, missing extensions, SBI vs SSTC timer).
7. Documentation & comments: stale comments, missing Safety annotations,
   inaccurate module header claims, mismatch between code and docs,
   dead code, unused variables/functions.

Output format (markdown):
## Executive summary
## Findings (severity-ranked: CRITICAL / HIGH / MEDIUM / LOW / INFO)
For each finding: severity, file:line, description, concrete fix suggestion.
Mark ALL findings as [NEW] — do NOT report previously fixed issues.
## What looks correct (so the author knows what not to change)
## Blind spots & suggestions for the next milestone (M2: user processes / IPC
/ capabilities / multi-core)

Be specific. Cite file paths and line numbers. Do NOT modify any code.
Do NOT speculate beyond what the code shows. Flag anything suspicious even
if you cannot prove it is wrong. Be exhaustive — it is better to flag a
false positive than to miss a real bug. Think step by step through each
module, examining every unsafe block, every atomic operation, and every
state transition."""


def collect_files():
    found = {}
    for pattern in GLOBS:
        for path in glob.glob(os.path.join(REPO_ROOT, pattern), recursive=True):
            if os.path.isfile(path):
                found[os.path.relpath(path, REPO_ROOT)] = path
    return sorted(found.items())


def build_prompt(files):
    parts = [INSTRUCTIONS]
    parts.append("===== BEGIN SOURCE FILES =====")
    for rel, path in files:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            content = f.read()
        parts.append(f"===== FILE: {rel} =====")
        parts.append(content)
    parts.append("===== END SOURCE FILES =====")
    return "\n\n".join(parts)


def get_key(args):
    key = os.environ.get("IGNIUM_AUDIT_KEY", "").strip()
    if not key and args.key_file:
        with open(args.key_file, "r", encoding="utf-8") as f:
            key = f.read().strip()
    if not key:
        print("ERROR: no API key. Set IGNIUM_AUDIT_KEY or use --key-file.", file=sys.stderr)
        sys.exit(2)
    return key


def call_api(key, prompt, retries=3):
    body = json.dumps(
        {
            "model": MODEL,
            "messages": [
                {"role": "system", "content": "You are a senior OS kernel security reviewer."},
                {"role": "user", "content": prompt},
            ],
            "max_tokens": 393216,
            "reasoning_effort": EFFORT,
            "stream": False,
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        API_URL,
        data=body,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    last = None
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
                data = json.loads(resp.read().decode("utf-8"))
            msg = data["choices"][0]["message"]
            content = (msg.get("content") or "").strip()
            reasoning = (msg.get("reasoning_content") or "").strip()
            usage = data.get("usage", {})
            print(
                f"tokens: in={usage.get('prompt_tokens', '?')} "
                f"out={usage.get('completion_tokens', '?')} "
                f"(reasoning={len(reasoning)} chars)"
            )
            if not content:
                raise RuntimeError(
                    "empty final answer from model "
                    f"(reasoning produced {len(reasoning)} chars; "
                    "likely max_tokens exhausted by thinking)"
                )
            return content
        except urllib.error.HTTPError as e:
            last = e
            print(f"attempt {attempt + 1}: HTTP {e.code}", file=sys.stderr)
            if e.code in (401, 403):
                break
        except (urllib.error.URLError, TimeoutError) as e:
            last = e
            print(f"attempt {attempt + 1}: {e}", file=sys.stderr)
    raise SystemExit(f"API call failed: {last}")


def main():
    parser = argparse.ArgumentParser(description="Independent AI audit of Ignium kernel")
    parser.add_argument("--key-file", help="one-shot file containing the API key")
    args = parser.parse_args()

    key = get_key(args)
    files = collect_files()
    print(f"collecting {len(files)} files from {REPO_ROOT}")
    prompt = build_prompt(files)
    print(f"prompt size: {len(prompt)} chars; calling {MODEL} ...")

    report = call_api(key, prompt)

    out_dir = os.path.join(REPO_ROOT, "docs", "audit-reports")
    os.makedirs(out_dir, exist_ok=True)
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    out_path = os.path.join(out_dir, f"{stamp}-{MODEL}.md")
    header = (
        f"# Independent AI audit ({MODEL})\n\n"
        f"- date: {stamp}\n- model: {MODEL}\n"
        f"- tool: scripts/ai_audit.py (external reviewer, not the author)\n\n"
    )
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(header + report)
    print(f"\nreport saved: {out_path}")


if __name__ == "__main__":
    main()