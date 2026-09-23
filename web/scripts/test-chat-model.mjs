import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const webRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const output = mkdtempSync(join(tmpdir(), "cheaptrip-web-tests-"));
const tsc = join(webRoot, "node_modules", ".bin", "tsc");

try {
  execFileSync(
    tsc,
    [
      "--target",
      "ES2022",
      "--ignoreConfig",
      "--module",
      "Node16",
      "--moduleResolution",
      "Node16",
      "--strict",
      "--skipLibCheck",
      "--rootDir",
      "src",
      "--outDir",
      output,
      "src/features/chat/media.ts",
      "src/features/chat/model.ts",
      "src/features/chat/model.test.ts",
      "src/features/chat/sessionCreation.ts",
      "src/features/chat/sessionCreation.test.ts",
    ],
    { cwd: webRoot, stdio: "inherit" },
  );
  writeFileSync(join(output, "package.json"), '{"type":"module"}\n');
  execFileSync(process.execPath, [join(output, "features/chat/model.test.js")], {
    cwd: webRoot,
    stdio: "inherit",
  });
  execFileSync(process.execPath, [join(output, "features/chat/sessionCreation.test.js")], {
    cwd: webRoot,
    stdio: "inherit",
  });
} finally {
  rmSync(output, { recursive: true, force: true });
}
