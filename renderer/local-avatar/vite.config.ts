// Relative base so the bundle works under any path the Rust host serves it
// from. No inline script is emitted: the host's CSP is `script-src 'self'`.
// Two entry pages share one build: the avatar renderer (index.html) and the
// local operation panel (panel.html, issue #335, no extra dependency).
export default {
  base: "./",
  build: {
    outDir: "dist",
    target: "es2022",
    modulePreload: { polyfill: false },
    chunkSizeWarningLimit: 1000,
    rollupOptions: {
      input: { index: "index.html", panel: "panel.html" },
    },
  },
};
