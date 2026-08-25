// 将私有仓库 LoopMaster 的公开内容同步到公开镜像仓库 LoopMaster-public。
// 用法: node scripts/export-public.mjs
// 设计原则: 单向同步 (私有 -> 公开)，绝不把私有内容 (Doc/、.env、secrets 等) 带出。

import { promises as fs } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const PRIVATE_ROOT = path.resolve(__dirname, '..');
// 公开仓库默认位置 (与私有仓库同级)。可用环境变量覆盖: PUBLIC_REPO_PATH=xxx
const PUBLIC_ROOT = process.env.PUBLIC_REPO_PATH
  ? path.resolve(process.env.PUBLIC_REPO_PATH)
  : path.resolve(PRIVATE_ROOT, '..', 'LoopMaster-public');

// 需要同步到公开仓库的目录/文件 (白名单)
const INCLUDE = [
  'app-service',
  'audio-core',
  'audio-windows',
  'diagnostics',
  'docs',
  'frontend',
  'Cargo.toml',
  'Cargo.lock',
  'README.md',
  'CONTRIBUTING.md',
  'LICENSE',
  '.gitignore',
];

// 同步时需要剔除的私有/产物路径片段 (相对各 include 根)
const EXCLUDE_FRAGMENTS = [
  'target',
  'node_modules',
  'dist',
  '.tauri',
  'dist-installer',
  '.env',
  '.env.',
  '.user',
  '.local',
  '.log',
  'Doc',
];

function isExcluded(relPath) {
  const parts = relPath.split(path.sep);
  return parts.some((p) => EXCLUDE_FRAGMENTS.includes(p));
}

async function collectFiles(dir, base, out) {
  const entries = await fs.readdir(dir, { withFileTypes: true });
  for (const e of entries) {
    const abs = path.join(dir, e.name);
    const rel = path.relative(base, abs);
    if (isExcluded(rel)) continue;
    if (e.isDirectory()) {
      await collectFiles(abs, base, out);
    } else if (e.isFile()) {
      out.push({ abs, rel });
    }
  }
}

async function rimraf(target) {
  await fs.rm(target, { recursive: true, force: true });
}

async function main() {
  // 校验公开仓库存在且是个 git 仓库
  try {
    const stat = await fs.stat(path.join(PUBLIC_ROOT, '.git'));
    if (!stat.isDirectory()) throw new Error('.git not a dir');
  } catch {
    console.error(`[export-public] 公开仓库不存在或缺少 .git: ${PUBLIC_ROOT}`);
    console.error('请先克隆公开仓库: git clone <url> LoopMaster-public');
    process.exit(1);
  }

  // 1. 清空公开仓库中属于白名单的目录/文件 (保留 .git 及其它未列项)
  for (const item of INCLUDE) {
    const target = path.join(PUBLIC_ROOT, item);
    await rimraf(target);
  }

  // 2. 从私有仓库收集白名单文件
  const files = [];
  for (const item of INCLUDE) {
    const src = path.join(PRIVATE_ROOT, item);
    try {
      const st = await fs.stat(src);
      if (st.isDirectory()) {
        await collectFiles(src, PRIVATE_ROOT, files);
      } else if (st.isFile()) {
        files.push({ abs: src, rel: item });
      }
    } catch {
      console.warn(`[export-public] 跳过不存在的项: ${item}`);
    }
  }

  // 3. 写入公开仓库
  let copied = 0;
  for (const { abs, rel } of files) {
    const dest = path.join(PUBLIC_ROOT, rel);
    await fs.mkdir(path.dirname(dest), { recursive: true });
    await fs.copyFile(abs, dest);
    copied++;
  }

  console.log(`[export-public] 已同步 ${copied} 个文件到:`);
  console.log(`  ${PUBLIC_ROOT}`);
  console.log('下一步:');
  console.log(`  cd "${PUBLIC_ROOT}"`);
  console.log('  git add -A && git commit -m "chore: 同步更新" && git push');
}

main().catch((err) => {
  console.error('[export-public] 失败:', err);
  process.exit(1);
});
