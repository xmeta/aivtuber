// Relative base so the bundle works under any path the Rust host serves it
// from. No inline script is emitted: the host's CSP is `script-src 'self'`.
export default {
  base: "./",
  build: {
    outDir: "dist",
    target: "es2022",
    modulePreload: { polyfill: false },
    chunkSizeWarningLimit: 1000,
  },
};
