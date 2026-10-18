// Builds the frontend into dist/: compiles ui/*.ts with tsc and copies static files and fonts.
import { execFileSync } from "node:child_process";
import { cpSync, mkdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");
const fonts = join(root, "node_modules", "geist", "dist", "fonts");

rmSync(dist, { recursive: true, force: true });
mkdirSync(join(dist, "fonts"), { recursive: true });

execFileSync(process.execPath, [join(root, "node_modules", "typescript", "bin", "tsc"), "-p", root], {
  stdio: "inherit",
});

for (const file of ["index.html", "styles.css"]) {
  cpSync(join(root, "ui", file), join(dist, file));
}
for (const [dir, file] of [
  ["geist-sans", "Geist-Regular.woff2"],
  ["geist-sans", "Geist-Medium.woff2"],
  ["geist-mono", "GeistMono-Regular.woff2"],
]) {
  cpSync(join(fonts, dir, file), join(dist, "fonts", file));
}
console.log("ui built into dist/");
