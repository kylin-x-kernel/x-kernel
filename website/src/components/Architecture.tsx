import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { useI18n } from "../i18n";
import { Section, fadeUp, stagger } from "./Section";

const AUTOPLAY_MS = 3600;

export function Architecture() {
  const { t } = useI18n();
  const layers = t.architecture.layers;
  const [activeIdx, setActiveIdx] = useState(2);
  const [isAutoplay, setIsAutoplay] = useState(true);

  useEffect(() => {
    if (!isAutoplay) return;
    const id = window.setInterval(() => setActiveIdx((i) => (i + 1) % layers.length), AUTOPLAY_MS);
    return () => window.clearInterval(id);
  }, [isAutoplay, layers.length]);

  const active = layers[activeIdx];

  return (
    <Section id="architecture" kicker={t.architecture.kicker} title={t.architecture.title} lead={t.architecture.lead}>
      <div className="arch-wrap">
        <motion.div
          className="stack"
          variants={stagger(0.09)}
          initial="hidden"
          whileInView="show"
          viewport={{ once: true, margin: "-60px" }}
          onPointerEnter={() => setIsAutoplay(false)}
        >
          {/* A syscall "packet" travelling down the stack, from userspace to hardware. */}
          <motion.span
            className="packet"
            aria-hidden
            animate={{ top: ["2%", "96%"], opacity: [0, 1, 1, 0] }}
            transition={{ duration: 3.2, repeat: Infinity, ease: "easeInOut", repeatDelay: 0.6 }}
          />
          {layers.map((l, i) => (
            <motion.button
              key={l.id}
              type="button"
              variants={{
                hidden: { opacity: 0, rotateX: -40, y: 30 },
                show: { opacity: 1, rotateX: 0, y: 0, transition: { duration: 0.7, ease: [0.22, 1, 0.36, 1] } },
              }}
              className={`layer ${i === activeIdx ? "active" : ""}`}
              onClick={() => {
                setActiveIdx(i);
                setIsAutoplay(false);
              }}
              onMouseEnter={() => setActiveIdx(i)}
              aria-pressed={i === activeIdx}
            >
              {i === activeIdx && <motion.span layoutId="layer-bar" className="bar" />}
              <span className="idx">L{layers.length - 1 - i}</span>
              <span className="nm">{l.name}</span>
              <span className="chips">{l.crates.slice(0, 3).join(" · ")}</span>
            </motion.button>
          ))}
        </motion.div>

        <motion.div
          className="panel arch-detail"
          initial={{ opacity: 0, x: 30 }}
          whileInView={{ opacity: 1, x: 0 }}
          viewport={{ once: true }}
          transition={{ duration: 0.8, ease: [0.22, 1, 0.36, 1] }}
        >
          <span className="lbl">{t.architecture.hint}</span>
          <AnimatePresence mode="wait">
            <motion.div
              key={active.id}
              initial={{ opacity: 0, y: 14 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: -14 }}
              transition={{ duration: 0.3 }}
            >
              <h3>{active.name}</h3>
              <p>{active.summary}</p>
              <span className="lbl">{t.architecture.crateLabel}</span>
              <motion.div className="crate-list" variants={stagger(0.04)} initial="hidden" animate="show">
                {active.crates.map((c) => (
                  <motion.code
                    key={c}
                    className="crate"
                    variants={{ hidden: { opacity: 0, scale: 0.8 }, show: { opacity: 1, scale: 1 } }}
                  >
                    {c}
                  </motion.code>
                ))}
              </motion.div>
            </motion.div>
          </AnimatePresence>
        </motion.div>
      </div>

      <motion.div
        className="pillars"
        variants={stagger(0.1)}
        initial="hidden"
        whileInView="show"
        viewport={{ once: true, margin: "-40px" }}
      >
        {t.architecture.pillars.map((p, i) => (
          <motion.div className="pillar" key={p.title} variants={fadeUp}>
            <span className="n">0{i + 1}</span>
            <h4>{p.title}</h4>
            <p>{p.body}</p>
          </motion.div>
        ))}
      </motion.div>
    </Section>
  );
}
