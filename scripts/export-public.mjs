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
  // 远程控制台源码必须随公开仓库同步；dist 仍由 EXCLUDE_FRAGMENTS 排除并在构建时生成。
  'frontend-remote',
  // 公开仓库构建入口（README 引用）；export-public.mjs 属私有仓库工具，不导出。
  'scripts/build-remote.mjs',
  'Cargo.toml',
  'Cargo.lock',
  'README.md',
  'CONTRIBUTING.md',
  'LICENSE',
  '.gitignore',
];

// 同步时需要剔除的私有/产物目录名 (相对各 include 根)
const EXCLUDED_DIR_NAMES = new Set([
  'target',
  'node_modules',
  'dist',
  '.tauri',
  'dist-installer',
  '.local',
  'Doc',
]);

const PRIVATE_KEY_EXTENSIONS = new Set(['.key', '.pem', '.p12', '.pfx']);

function isExcluded(relPath) {
  const parts = relPath.split(path.sep);
  if (parts.some((part) => EXCLUDED_DIR_NAMES.has(part))) return true;

  const basename = parts.at(-1)?.toLowerCase() ?? '';
  if (basename === 'agents.md') return true;
  if (basename === '.env' || basename.startsWith('.env.')) return true;
  if (basename.endsWith('.user') || basename.endsWith('.log')) return true;
  if (basename === 'id_rsa' || basename === 'id_ed25519') return true;
  if (basename.includes('private-key') || basename.includes('private_key')) return true;
  return PRIVATE_KEY_EXTENSIONS.has(path.extname(basename));
}

function checkExclusionRules() {
  const mustExclude = [
    'AGENTS.md',
    'frontend-remote/dist/index.html',
    'frontend-remote/node_modules/pkg/index.js',
    'frontend-remote/.env',
    'frontend-remote/.env.local',
    'frontend-remote/certs/server.key',
    'frontend-remote/certs/server.pem',
    'frontend-remote/certs/server.p12',
    'frontend-remote/certs/server.pfx',
    'frontend-remote/certs/private_key.der',
  ];
  const mustInclude = [
    'frontend-remote/package.json',
    'frontend-remote/src/main.tsx',
    'frontend-remote/src/key-map.ts',
  ];

  const failures = [
    ...mustExclude.filter((candidate) => !isExcluded(path.normalize(candidate))),
    ...mustInclude.filter((candidate) => isExcluded(path.normalize(candidate))),
  ];
  if (failures.length > 0) {
    throw new Error(`导出过滤规则自测失败: ${failures.join(', ')}`);
  }
  console.log('[export-public] 导出过滤规则自测通过');
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

function pathsOverlap(left, right) {
  const relative = path.relative(left, right);
  return relative === '' || (!relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative));
}

async function validateRepositoryIsolation() {
  const privateReal = await fs.realpath(PRIVATE_ROOT);
  const publicReal = await fs.realpath(PUBLIC_ROOT);
  if (pathsOverlap(privateReal, publicReal) || pathsOverlap(publicReal, privateReal)) {
    throw new Error(`公开仓库必须与私有仓库完全隔离: private=${privateReal}, public=${publicReal}`);
  }
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
  await validateRepositoryIsolation();

  // 删除目标前先完整收集源文件，避免配置或权限问题导致目标被清空后才失败。
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

  // 1. 清空公开仓库中属于白名单的目录/文件 (保留 .git 及其它未列项)
  for (const item of INCLUDE) {
    const target = path.join(PUBLIC_ROOT, item);
    await rimraf(target);
  }

  // 2. 写入公开仓库
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

const command = process.argv[2];
const operation = command === '--check-rules' ? Promise.resolve().then(checkExclusionRules) : main();

operation.catch((err) => {
  console.error('[export-public] 失败:', err);
  process.exit(1);
});
