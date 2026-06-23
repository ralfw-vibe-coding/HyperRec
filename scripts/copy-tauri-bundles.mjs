import { cp, copyFile, mkdir, readFile, readdir, rm, stat } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";

const repoRoot = process.cwd();
const cargoConfigPath = path.join(repoRoot, "src-tauri", ".cargo", "config.toml");
const outputDir = path.join(repoRoot, "dist-app");

function resolveTargetDir(config) {
  const match = config.match(/^\s*target-dir\s*=\s*"([^"]+)"\s*$/m);
  if (!match) {
    return path.join(repoRoot, "src-tauri", "target");
  }

  const configured = match[1].replace(/^~/, process.env.HOME ?? "~");
  return path.isAbsolute(configured) ? configured : path.join(repoRoot, "src-tauri", configured);
}

async function newest(paths) {
  let selected = null;

  for (const filePath of paths) {
    const fileStat = await stat(filePath);
    if (!selected || fileStat.mtimeMs > selected.mtimeMs) {
      selected = { filePath, mtimeMs: fileStat.mtimeMs };
    }
  }

  return selected?.filePath;
}

async function globDir(dir, suffix) {
  if (!existsSync(dir)) return [];

  const entries = await readdir(dir, { withFileTypes: true });
  return entries
    .filter((entry) => entry.name.endsWith(suffix))
    .map((entry) => path.join(dir, entry.name));
}

const cargoConfig = existsSync(cargoConfigPath) ? await readFile(cargoConfigPath, "utf8") : "";
const targetDir = resolveTargetDir(cargoConfig);
const bundleDir = path.join(targetDir, "release", "bundle");

const appPath = await newest(await globDir(path.join(bundleDir, "macos"), ".app"));
const dmgPath = await newest(await globDir(path.join(bundleDir, "dmg"), ".dmg"));

if (!appPath && !dmgPath) {
  throw new Error(`No Tauri bundles found in ${bundleDir}. Run "npm run tauri build" first.`);
}

await mkdir(outputDir, { recursive: true });

if (appPath) {
  const destination = path.join(outputDir, path.basename(appPath));
  await rm(destination, { recursive: true, force: true });
  await cp(appPath, destination, { recursive: true, force: true, verbatimSymlinks: true });
  console.log(`Copied ${appPath} -> ${destination}`);
}

if (dmgPath) {
  const destination = path.join(outputDir, path.basename(dmgPath));
  await copyFile(dmgPath, destination);
  console.log(`Copied ${dmgPath} -> ${destination}`);
}
