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
# 推理强度:max 最彻底(耗时最长,可达 20+ 分钟);high 较快。
# 按项目要求,默认使用 max;可经 env 临时调低。
EFFORT = os.environ.get("IGNIUM_AUDIT_EFFORT", "max")
TIMEOUT = 900

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
incomplete. Your job is to find what the author missed.

Project context:
- Language: Rust (no_std + alloc), edition 2021, toolchain pinned 1.97.1
- Architecture: RISC-V 64 (riscv64gc), running in S-mode on QEMU virt
  machine (OpenSBI 1.3 firmware), kernel linked at 0x80200000
- Stage: **M1 complete** (trap/timer/buddy/Sv39 paging/kernel heap/
  cooperative+preemptive scheduler/Mutex+Condvar). Boot runs a full
  selftest suite: buddy, Sv39, heap, scheduler (cooperative + tick-based
  preemption), sync primitives; then idle loop with 1s uptime heartbeat.
- Timer uses SSTC (direct stimecmp writes); timer-driven preemption
  switches threads via full trap-frame swap in the ISR (frame_valid
  protocol). Threads: kernel-mode only, cooperative yield via callee-saved
  context switch.
- No MMU page permissions split yet (RWX identity map; D2 deferred).
- Future direction: microkernel (user processes/IPC/capabilities) with
  OpenHarmony-compatible userspace layer. M1.5 (stabilization) planned:
  FDT parsing, permission split, guard pages, RVA23 P1. Audit with that
  trajectory in mind.

Review requirements - examine from ALL of the following perspectives:
1. Memory safety: UB, unsafe misuse, volatile access, pointer arithmetic,
   alignment, integer overflow, dangling references.
2. Control flow & state machine: boot sequence, panic paths, infinite loops,
   unreachable states, catastrophic hang scenarios.
3. Concurrency & reentrancy: atomic ordering, ISR-safety of log paths,
   double-fault recursion, future interrupt context hazards.
4. Threat model: hostile bootloader, corrupted RAM, register/CSR misuse,
   information leakage, privilege issues, attacker-influenced values.
5. Build & reproducibility: linker script correctness, orphan sections,
   toolchain drift, CI pitfalls, debug-vs-release divergence.
6. Hidden bugs: things that work in QEMU but would break on real hardware.

Output format (markdown):
## Executive summary
## Findings (severity-ranked: CRITICAL / HIGH / MEDIUM / LOW / INFO)
For each finding: severity, file:line, description, concrete fix suggestion.
## What looks correct (so the author knows what not to change)
## Blind spots & suggestions for the next milestone (M1: trap handling)

Be specific. Cite file paths and line numbers. Do NOT modify any code.
Do NOT speculate beyond what the code shows. Flag anything suspicious even
if you cannot prove it is wrong.
"""


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
            # 输出不设实际限制:max 推理会吃掉大量 token(实测单次
            # 思考可达 30 万字符),代码量增长后只会更多。
            # 上限取 API 最大值 384K token(实际不可能触达)。
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
