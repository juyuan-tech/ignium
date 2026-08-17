// 构建脚本:向链接器传递**绝对路径**的链接脚本(L1)。
// 相对路径(-Tkernel/linker.ld)依赖 cargo 工作目录,从子目录
// 或 IDE 构建会解析失败;CARGO_MANIFEST_DIR 保证任意调用方式一致。
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    // 变更链接脚本时重跑(否则增量构建可能不重新链接)。
    println!("cargo:rerun-if-changed=linker.ld");
}
