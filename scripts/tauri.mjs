import { spawn } from "node:child_process";
import path from "node:path";
import process from "node:process";

const args = process.argv.slice(2);
const tauriBin = path.join(process.cwd(), "node_modules", ".bin", "tauri");

function run(command, commandArgs) {
  return new Promise((resolve) => {
    const child = spawn(command, commandArgs, { stdio: "inherit" });
    child.on("exit", (code, signal) => resolve({ code, signal }));
  });
}

const result = await run(tauriBin, args);

if (result.signal) {
  process.kill(process.pid, result.signal);
}

if (result.code !== 0) {
  process.exit(result.code ?? 1);
}

if (args[0] === "build") {
  const copyScript = path.join(process.cwd(), "scripts", "copy-tauri-bundles.mjs");
  const copyResult = await run(process.execPath, [copyScript]);
  process.exit(copyResult.code ?? 1);
}
