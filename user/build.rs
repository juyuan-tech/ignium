// 构建脚本:向链接器传递**绝对路径**的链接脚本(与 kernel/build.rs 同约定,
// 保证任意调用方式 / 任意工作目录一致)。
fn main() {
    // 链接参数依赖 CARGO_MANIFEST_DIR(输出含其绝对路径)。显式声明环境依赖,
    // 否则 cargo 指纹不含该 env:主机构建(docker 外 `make build`,路径形如
    // `/home/...`)与容器构建(`/work`)共享同一 `target/`,会**跨环境复用**陈旧
    // 输出 —— 容器内以主机绝对路径找链接脚本而失败。声明后每次环境切换必
    // 重跑本脚本,输出恒指向真实文件。
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    // 变更链接脚本时重跑(否则增量构建可能不重新链接)。
    println!("cargo:rerun-if-changed=linker.ld");
}
