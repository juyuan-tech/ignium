// 构建脚本:向链接器传递**绝对路径**的链接脚本(L1)。
// 相对路径(-Tkernel/linker.ld)依赖 cargo 工作目录,从子目录
// 或 IDE 构建会解析失败;CARGO_MANIFEST_DIR 保证任意调用方式一致。
//
// M3 T1:额外编译用户测试程序(user/ 独立 crate),产物拷入 OUT_DIR 供
// kernel `include_bytes!` 内嵌(方案 A,见 M3-DESIGN §3.5)。
use std::path::{Path, PathBuf};

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    // 变更链接脚本时重跑(否则增量构建可能不重新链接)。
    println!("cargo:rerun-if-changed=linker.ld");
    build_user_elf(&dir);
}

/// M3 T1:编译 user/ 独立 crate → `$OUT_DIR/hello.elf`(kernel include_bytes!)。
///
/// 方案 A(cargo-in-cargo):env `CARGO_TARGET_DIR=$OUT_DIR/user-target`
/// 隔离内层 cargo 的 target 目录,规避 cargo-in-cargo 锁冲突;外层 cargo
/// 经 `CARGO` 环境变量指向同一二进制,无需在 PATH 上定位。
/// 用户程序无外部依赖(no_std + 仅 core),CI/离线可复现。
fn build_user_elf(kernel_dir: &str) {
    let user_dir = Path::new(kernel_dir).join("../user");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let target_dir = out_dir.join("user-target");

    // 用户源码/脚本/链接脚本任一变更 → 重跑 build.rs(增量重编译用户程序)。
    println!(
        "cargo:rerun-if-changed={}",
        user_dir.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        user_dir.join("build.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        user_dir.join("linker.ld").display()
    );
    println!("cargo:rerun-if-changed={}", user_dir.join("src").display());

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = std::process::Command::new(&cargo);
    cmd.args([
        "build",
        "--manifest-path",
        user_dir.join("Cargo.toml").to_str().expect("utf8"),
        "--target",
        "riscv64gc-unknown-none-elf",
        "--release",
    ])
    .env("CARGO_TARGET_DIR", &target_dir);
    // 隔离外层 clippy/rustc 包装:内层恒为**普通构建**。外层 `cargo clippy`
    // 会经 RUSTC_WORKSPACE_WRAPPER=clippy-driver 把用户 crate 也纳入 clippy
    // 检查(空 loop 等 lint 误伤);显式剥离包装器,内层永远用真 rustc 编译。
    cmd.env_remove("RUSTC_WORKSPACE_WRAPPER");
    cmd.env_remove("CLIPPY_ARGS");
    let status = cmd.status().expect("failed to run cargo for user program");
    assert!(
        status.success(),
        "user program (ignium-user-hello) build failed"
    );

    let elf = target_dir
        .join("riscv64gc-unknown-none-elf")
        .join("release")
        .join("ignium-user-hello");
    std::fs::copy(&elf, out_dir.join("hello.elf"))
        .expect("copy user ELF into OUT_DIR for include_bytes!");
}
