import { readFileSync } from "node:fs";
import vm from "node:vm";

const html = readFileSync(new URL("../dist/index.html", import.meta.url), "utf8");
const scripts = [...html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/g)];

if (!html.includes('<div id="app"></div>')) {
  throw new Error("self-contained inspector artifact is missing its mount point");
}
if (scripts.length !== 1) {
  throw new Error(`expected one inline inspector script, found ${scripts.length}`);
}
if (/\b(?:src|href)=["']https?:\/\//i.test(html)) {
  throw new Error("inspector artifact contains an external runtime asset");
}

new vm.Script(scripts[0][1], { filename: "dist/index.html:inline-script" });
