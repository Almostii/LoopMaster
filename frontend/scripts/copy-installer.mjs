// 将 Tauri 生成的安装包统一拷贝到仓库根目录的 dist-installer/ 文件夹
import { existsSync, mkdirSync, copyFileSync, readFileSync, readdirSync, rmSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
// frontend/scripts -> 仓库根目录
const root = join(__dirname, '..', '..');
const src = join(root, 'frontend', 'src-tauri', 'target', 'release', 'bundle');
const dest = join(root, 'dist-installer');
const packageJson = JSON.parse(readFileSync(join(root, 'frontend', 'package.json'), 'utf8'));
const version = packageJson.version;

if (!existsSync(src)) {
  console.error('未找到 bundle 输出目录:', src);
  process.exit(1);
}

mkdirSync(dest, { recursive: true });

const exts = ['.exe', '.msi'];
const installers = [];
for (const dir of readdirSync(src)) {
  const sub = join(src, dir);
  if (!statSync(sub).isDirectory()) continue;
  for (const f of readdirSync(sub)) {
    const extension = f.slice(f.lastIndexOf('.')).toLowerCase();
    if (exts.includes(extension) && f.includes(`_${version}_`)) {
      installers.push({ source: join(sub, f), name: f });
    }
  }
}

if (installers.length === 0) {
  console.error(`未找到版本 ${version} 的安装包文件`);
  process.exit(1);
}

for (const file of readdirSync(dest)) {
  if (exts.includes(file.slice(file.lastIndexOf('.')).toLowerCase())) {
    rmSync(join(dest, file));
  }
}
for (const installer of installers) {
  copyFileSync(installer.source, join(dest, installer.name));
  console.log('已拷贝:', installer.name);
}
console.log(`完成：共 ${installers.length} 个安装包已输出到 ${dest}`);
