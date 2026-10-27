// Builds the frontend into dist/: compiles ui/*.ts with tsc and copies static files and fonts.
import { execFileSync } from "node:child_process";
import { cpSync, mkdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");
const fonts = join(root, "node_modules", "@fontsource-variable", "inter", "files");

rmSync(dist, { recursive: true, force: true });
mkdirSync(join(dist, "fonts"), { recursive: true });

execFileSync(process.execPath, [join(root, "node_modules", "typescript", "bin", "tsc"), "-p", root], {
  stdio: "inherit",
});

for (const file of ["index.html", "styles.css"]) {
  cpSync(join(root, "ui", file), join(dist, file));
}
for (const file of ["inter-latin-wght-normal.woff2", "inter-cyrillic-wght-normal.woff2"]) {
  cpSync(join(fonts, file), join(dist, "fonts", file));
}
console.log("ui built into dist/");
