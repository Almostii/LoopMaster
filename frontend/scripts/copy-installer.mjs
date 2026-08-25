// 将 Tauri 生成的安装包统一拷贝到仓库根目录的 dist-installer/ 文件夹
import { existsSync, mkdirSync, copyFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
// frontend/scripts -> 仓库根目录
const root = join(__dirname, '..', '..');
const src = join(root, 'frontend', 'src-tauri', 'target', 'release', 'bundle');
const dest = join(root, 'dist-installer');

if (!existsSync(src)) {
  console.error('未找到 bundle 输出目录:', src);
  process.exit(1);
}

mkdirSync(dest, { recursive: true });

const exts = ['.exe', '.msi'];
let copied = 0;
for (const dir of readdirSync(src)) {
  const sub = join(src, dir);
  if (!statSync(sub).isDirectory()) continue;
  for (const f of readdirSync(sub)) {
    if (exts.includes(f.slice(f.lastIndexOf('.')))) {
      copyFileSync(join(sub, f), join(dest, f));
      console.log('已拷贝:', f);
      copied++;
    }
  }
}

if (copied === 0) {
  console.warn('未找到任何安装包文件');
} else {
  console.log(`完成：共 ${copied} 个安装包已输出到 ${dest}`);
}
