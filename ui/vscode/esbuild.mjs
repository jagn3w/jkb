import esbuild from "esbuild";

const watch = process.argv.includes("--watch");

/** Bundle the extension host into a single CommonJS file; `vscode` stays external. */
const options = {
  entryPoints: ["src/extension.ts"],
  bundle: true,
  outfile: "dist/extension.js",
  external: ["vscode"],
  format: "cjs",
  platform: "node",
  target: "node18",
  sourcemap: true,
  logLevel: "info",
};

if (watch) {
  const ctx = await esbuild.context(options);
  await ctx.watch();
  console.log("esbuild: watching…");
} else {
  await esbuild.build(options);
}
