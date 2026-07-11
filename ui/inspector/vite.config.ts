import { readdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { defineConfig, type Plugin } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

function inlineArtifacts(): Plugin {
  return {
    name: "inline-inspector-artifacts",
    closeBundle() {
      const dist = resolve("dist");
      const htmlPath = resolve(dist, "index.html");
      let html = readFileSync(htmlPath, "utf8");
      const assets = readdirSync(resolve(dist, "assets"));
      const cssPath = resolve(dist, "assets", assets.find((name) => name.endsWith(".css"))!);
      const jsPath = resolve(dist, "assets", assets.find((name) => name.endsWith(".js"))!);
      const css = readFileSync(cssPath, "utf8");
      // Keep the generated whitespace table semantically identical while
      // avoiding trailing whitespace in the tracked one-file artifact.
      const js = readFileSync(jsPath, "utf8").replace(/\t\n/g, "\\t\\n");
      html = html.replace(/<link rel="stylesheet"[^>]+>/, `<style>${css}</style>`);
      html = html.replace(/<script type="module"[^>]+><\/script>/, `<script>${js}</script>`);
      writeFileSync(htmlPath, html);
      unlinkSync(cssPath);
      unlinkSync(jsPath);
    }
  };
}

export default defineConfig({
  plugins: [svelte(), inlineArtifacts()],
  build: {
    cssCodeSplit: false,
    emptyOutDir: true,
    rollupOptions: {
      output: {
        assetFileNames: "assets/[name][extname]",
        entryFileNames: "assets/index.js",
        manualChunks: undefined
      }
    }
  }
});
