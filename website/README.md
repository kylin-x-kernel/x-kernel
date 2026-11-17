# x-kernel.com

Project showcase site for X-Kernel. React 18 + TypeScript + Vite, animations
with framer-motion and a hand-written canvas background. Bilingual (EN / 中文),
language follows the browser and is remembered per visitor.

```bash
cd website
npm install
npm run dev        # http://localhost:5173
npm run build      # type-check + static build into dist/
npm run preview    # serve dist/ locally
```

`dist/` is a fully static site: deploy it to any static host (Nginx, Caddy,
object storage + CDN). No server-side runtime is needed.

## Layout

| Path | Role |
|---|---|
| `src/content.ts` | All copy (EN + ZH) and shared facts: stats, platforms, links, quick-start commands. Edit text here. |
| `src/i18n.tsx` | Language context and persistence. |
| `src/components/` | One component per page section. |
| `src/components/KernelCanvas.tsx` | Hero node-graph animation (pauses off-screen, respects reduced motion). |
| `src/styles.css` | Design tokens and all styles. |
| `public/images/` | Screenshots copied from `docs/images/`. |

## Keeping facts accurate

Numbers and capability states mirror the top-level `README.md`
(Capability Status, Platform Support). The hero stats were measured from the
tree when the page was written: ~213 workspace crates, ~281 syscall dispatch
entries in `core/ksyscall/src/dispatch.rs`, ~740K lines of Rust outside
`vendor/`. Update `shared.stats` in `src/content.ts` when they drift.

The hero boot log and the workload terminal output are stylized
illustrations, not captured logs.
