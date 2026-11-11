// Builds both artifacts from the single version in package.json:
//   plugin/dist/com.avcompanion.control.streamDeckPlugin
//   src-tauri/target/release/bundle/...   (msi on Windows, dmg on macOS)
// Usage: node scripts/release.mjs [--plugin-only]
import { execSync } from "node:child_process";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";

const pluginOnly = process.argv.includes("--plugin-only");
const run = (cmd) => {
  console.log(`\n> ${cmd}`);
  execSync(cmd, { stdio: "inherit" });
};
const readJson = (p) => JSON.parse(readFileSync(p, "utf8"));
const writeJson = (p, v) => writeFileSync(p, JSON.stringify(v, null, 2) + "\n");

const { version } = readJson("package.json");
const pluginDir = "plugin/com.avcompanion.control.sdPlugin";

// Keep every version field in step with package.json.
const conf = readJson("src-tauri/tauri.conf.json");
conf.version = version;
writeJson("src-tauri/tauri.conf.json", conf);
const manifest = readJson(`${pluginDir}/manifest.json`);
manifest.Version = `${version}.0`; // Stream Deck versions are four-part
writeJson(`${pluginDir}/manifest.json`, manifest);

run("node --test plugin/test/plugin.test.js");
run(`npx streamdeck validate ${pluginDir}`);
mkdirSync("plugin/dist", { recursive: true });
run(`npx streamdeck pack ${pluginDir} --output plugin/dist --force`);

if (!pluginOnly) {
  run("cargo test --manifest-path src-tauri/Cargo.toml -p av-core");
  run("npx tauri build");
}
console.log("\nDone.");
