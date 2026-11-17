import { useEffect, useRef } from "react";

interface Node {
  x: number;
  y: number;
  vx: number;
  vy: number;
  r: number;
  hue: number;
}

interface Pulse {
  from: number;
  to: number;
  t: number;
  speed: number;
}

const LINK_DIST_PX = 140;
const MOUSE_RADIUS_PX = 180;

/**
 * Animated node graph behind the hero: drifting "crates" linked by edges,
 * with pulses travelling along edges like messages crossing API contracts.
 */
export function KernelCanvas() {
  const ref = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const isReducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    let width = 0;
    let height = 0;
    let nodes: Node[] = [];
    let pulses: Pulse[] = [];
    let frame = 0;
    let isVisible = true;
    const mouse = { x: -9999, y: -9999 };

    const resize = () => {
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      width = canvas.clientWidth;
      height = canvas.clientHeight;
      canvas.width = Math.floor(width * dpr);
      canvas.height = Math.floor(height * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      const count = Math.round(Math.min(110, Math.max(36, (width * height) / 14000)));
      nodes = Array.from({ length: count }, () => ({
        x: Math.random() * width,
        y: Math.random() * height,
        vx: (Math.random() - 0.5) * 0.35,
        vy: (Math.random() - 0.5) * 0.35,
        r: Math.random() * 1.6 + 0.8,
        hue: Math.random() < 0.7 ? 188 : 275,
      }));
      pulses = [];
    };

    const spawnPulse = () => {
      const from = Math.floor(Math.random() * nodes.length);
      let best = -1;
      let bestDist = Infinity;
      for (let i = 0; i < nodes.length; i++) {
        if (i === from) continue;
        const d = Math.hypot(nodes[i].x - nodes[from].x, nodes[i].y - nodes[from].y);
        if (d < LINK_DIST_PX && d < bestDist && Math.random() > 0.3) {
          best = i;
          bestDist = d;
        }
      }
      if (best >= 0) pulses.push({ from, to: best, t: 0, speed: 0.012 + Math.random() * 0.02 });
    };

    const draw = () => {
      ctx.clearRect(0, 0, width, height);

      for (const n of nodes) {
        if (!isReducedMotion) {
          const dx = n.x - mouse.x;
          const dy = n.y - mouse.y;
          const d = Math.hypot(dx, dy);
          if (d < MOUSE_RADIUS_PX && d > 0) {
            const force = (1 - d / MOUSE_RADIUS_PX) * 0.6;
            n.x += (dx / d) * force;
            n.y += (dy / d) * force;
          }
          n.x += n.vx;
          n.y += n.vy;
        }
        if (n.x < 0 || n.x > width) n.vx *= -1;
        if (n.y < 0 || n.y > height) n.vy *= -1;
        n.x = Math.max(0, Math.min(width, n.x));
        n.y = Math.max(0, Math.min(height, n.y));
      }

      ctx.lineWidth = 1;
      for (let i = 0; i < nodes.length; i++) {
        for (let j = i + 1; j < nodes.length; j++) {
          const a = nodes[i];
          const b = nodes[j];
          const d = Math.hypot(a.x - b.x, a.y - b.y);
          if (d < LINK_DIST_PX) {
            const alpha = (1 - d / LINK_DIST_PX) * 0.22;
            ctx.strokeStyle = `rgba(125, 211, 252, ${alpha})`;
            ctx.beginPath();
            ctx.moveTo(a.x, a.y);
            ctx.lineTo(b.x, b.y);
            ctx.stroke();
          }
        }
      }

      for (const n of nodes) {
        const nearMouse = Math.hypot(n.x - mouse.x, n.y - mouse.y) < MOUSE_RADIUS_PX;
        ctx.fillStyle = `hsla(${n.hue}, 90%, ${nearMouse ? 75 : 62}%, ${nearMouse ? 1 : 0.75})`;
        ctx.beginPath();
        ctx.arc(n.x, n.y, nearMouse ? n.r * 1.8 : n.r, 0, Math.PI * 2);
        ctx.fill();
      }

      if (!isReducedMotion) {
        if (Math.random() < 0.12 && pulses.length < 26) spawnPulse();
        pulses = pulses.filter((p) => p.t <= 1);
        for (const p of pulses) {
          p.t += p.speed;
          const a = nodes[p.from];
          const b = nodes[p.to];
          if (!a || !b) continue;
          const x = a.x + (b.x - a.x) * p.t;
          const y = a.y + (b.y - a.y) * p.t;
          const g = ctx.createRadialGradient(x, y, 0, x, y, 10);
          g.addColorStop(0, "rgba(34, 211, 238, 0.95)");
          g.addColorStop(1, "rgba(34, 211, 238, 0)");
          ctx.fillStyle = g;
          ctx.beginPath();
          ctx.arc(x, y, 10, 0, Math.PI * 2);
          ctx.fill();
        }
      }
    };

    const loop = () => {
      if (isVisible) draw();
      frame = requestAnimationFrame(loop);
    };

    const onMove = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      mouse.x = e.clientX - rect.left;
      mouse.y = e.clientY - rect.top;
    };
    const onLeave = () => {
      mouse.x = -9999;
      mouse.y = -9999;
    };

    const io = new IntersectionObserver(([entry]) => {
      isVisible = entry.isIntersecting;
    });
    io.observe(canvas);

    resize();
    if (isReducedMotion) {
      draw();
    } else {
      frame = requestAnimationFrame(loop);
    }
    window.addEventListener("resize", resize);
    window.addEventListener("pointermove", onMove, { passive: true });
    document.addEventListener("pointerleave", onLeave);

    return () => {
      cancelAnimationFrame(frame);
      io.disconnect();
      window.removeEventListener("resize", resize);
      window.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerleave", onLeave);
    };
  }, []);

  return <canvas ref={ref} className="net" aria-hidden />;
}
