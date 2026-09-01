//! 构建期保障：`frontend-remote/dist` 不进 Git，但 `rust-embed` 要求该目录
//! 在编译期存在。
//!
//! 干净检出的正确构建顺序是先构建远程前端（`node scripts/build-remote.mjs`）
//! 再执行 Cargo；本脚本只在 dist 缺失时生成**占位页**，保证
//! `cargo build/test/clippy` 不隐式依赖开发机残留产物、CI 可从空目录验证顺序。
//! 占位页仅包含提示文案，不含任何功能。

use std::env;
use std::fs;
use std::path::PathBuf;

const PLACEHOLDER_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>LoopMaster Web Console</title>
    <style>
      body {
        font-family: system-ui, sans-serif;
        display: flex;
        align-items: center;
        justify-content: center;
        min-height: 100vh;
        margin: 0;
        background: #1b1e23;
        color: #e5e7eb;
      }
      main { text-align: center; max-width: 32rem; padding: 2rem; }
      code { color: #2dd5bd; }
    </style>
  </head>
  <body>
    <main>
      <h1>LoopMaster Web Console</h1>
      <p>远程前端产物尚未构建。请在仓库根目录运行：</p>
      <p><code>node scripts/build-remote.mjs</code></p>
      <p>然后重新编译并重启 LoopMaster。</p>
    </main>
  </body>
</html>
"#;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dist = manifest_dir.join("../frontend-remote/dist");
    if !dist.is_dir() {
        fs::create_dir_all(&dist).expect("创建 frontend-remote/dist 目录失败");
        fs::write(dist.join("index.html"), PLACEHOLDER_HTML).expect("写入前端占位页失败");
        println!(
            "cargo:warning=frontend-remote/dist 缺失，已生成占位页；正式产物请先运行 `node scripts/build-remote.mjs`"
        );
    }
    println!("cargo:rerun-if-changed=../frontend-remote/dist/index.html");
}
