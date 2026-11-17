import { useEffect, useRef, useState, type PointerEvent } from "react";
import { AnimatePresence, motion, useMotionValue, useScroll, useSpring, useTransform } from "framer-motion";
import { useI18n } from "../i18n";
import { shared } from "../content";
import { KernelCanvas } from "./KernelCanvas";
import { BootTerminal } from "./BootTerminal";
import { Counter } from "./Counter";
import { Slogan } from "./Slogan";
import { ArrowIcon, GitIcon } from "./Icons";

const ease = [0.22, 1, 0.36, 1] as const;

export function Hero() {
  const { t } = useI18n();
  const [wordIdx, setWordIdx] = useState(0);
  const ref = useRef<HTMLElement>(null);

  useEffect(() => {
    const id = window.setInterval(() => setWordIdx((i) => (i + 1) % t.hero.rotate.length), 2000);
    return () => window.clearInterval(id);
  }, [t.hero.rotate.length]);

  const { scrollYProgress } = useScroll({ target: ref, offset: ["start start", "end start"] });
  const textY = useTransform(scrollYProgress, [0, 1], [0, 120]);
  const termY = useTransform(scrollYProgress, [0, 1], [0, 60]);
  const fade = useTransform(scrollYProgress, [0, 0.8], [1, 0]);

  const rx = useMotionValue(0);
  const ry = useMotionValue(0);
  const srx = useSpring(rx, { stiffness: 120, damping: 18 });
  const sry = useSpring(ry, { stiffness: 120, damping: 18 });

  const onTilt = (e: PointerEvent<HTMLDivElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const px = (e.clientX - r.left) / r.width - 0.5;
    const py = (e.clientY - r.top) / r.height - 0.5;
    ry.set(px * 12);
    rx.set(-py * 10);
  };
  const resetTilt = () => {
    rx.set(0);
    ry.set(0);
  };

  return (
    <section className="hero" id="top" ref={ref}>
      <KernelCanvas />
      <div className="container hero-grid">
        <motion.div style={{ y: textY, opacity: fade }}>
          <motion.span
            className="badge"
            initial={{ opacity: 0, y: 12 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: 0.6, delay: 0.2, ease }}
          >
            <span className="dot" />
            {t.hero.badge}
          </motion.span>

          <Slogan />

          <motion.p
            className="rotator"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ delay: 0.7, duration: 0.6 }}
          >
            <span>{t.hero.rotatePrefix}</span>
            <span className="slot" aria-live="polite">
              <AnimatePresence mode="popLayout" initial={false}>
                <motion.span
                  key={t.hero.rotate[wordIdx]}
                  initial={{ y: "100%", opacity: 0, filter: "blur(4px)" }}
                  animate={{ y: "0%", opacity: 1, filter: "blur(0px)" }}
                  exit={{ y: "-100%", opacity: 0, filter: "blur(4px)" }}
                  transition={{ duration: 0.45, ease }}
                >
                  {t.hero.rotate[wordIdx]}
                </motion.span>
              </AnimatePresence>
            </span>
            <span>{t.hero.rotateSuffix}</span>
          </motion.p>

          <motion.p
            className="lead"
            initial={{ opacity: 0, y: 16 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ delay: 0.85, duration: 0.7, ease }}
          >
            {t.hero.lead}
          </motion.p>

          <motion.div
            className="cta-row"
            initial={{ opacity: 0, y: 16 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ delay: 1, duration: 0.7, ease }}
          >
            <a className="btn primary" href="#start">
              {t.hero.ctaPrimary}
              <ArrowIcon />
            </a>
            <a className="btn" href={shared.links.repo} target="_blank" rel="noreferrer">
              <GitIcon />
              {t.hero.ctaSecondary}
            </a>
          </motion.div>

          <motion.div
            className="stats"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ delay: 1.15, duration: 0.8 }}
          >
            {shared.stats.map((s, i) => (
              <div className="stat" key={s.key}>
                <Counter value={s.value} suffix={s.suffix} delay={1.2 + i * 0.12} />
                <span>{t.hero.stats[s.key]}</span>
              </div>
            ))}
          </motion.div>
        </motion.div>

        <motion.div
          className="hero-term"
          style={{ y: termY, perspective: 1200 }}
          initial={{ opacity: 0, scale: 0.94, y: 30 }}
          animate={{ opacity: 1, scale: 1, y: 0 }}
          transition={{ duration: 1, delay: 0.5, ease }}
          onPointerMove={onTilt}
          onPointerLeave={resetTilt}
        >
          <motion.div style={{ rotateX: srx, rotateY: sry, transformStyle: "preserve-3d" }}>
            <BootTerminal />
          </motion.div>
        </motion.div>
      </div>
      <div className="scroll-cue" aria-hidden>
        {t.hero.scroll}
        <i />
      </div>
    </section>
  );
}
