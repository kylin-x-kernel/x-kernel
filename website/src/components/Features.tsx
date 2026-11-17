import type { PointerEvent } from "react";
import { motion } from "framer-motion";
import { useI18n } from "../i18n";
import { Section, fadeUp, stagger } from "./Section";
import { FeatureIcon } from "./Icons";

function trackPointer(e: PointerEvent<HTMLElement>) {
  const el = e.currentTarget;
  const r = el.getBoundingClientRect();
  el.style.setProperty("--mx", `${e.clientX - r.left}px`);
  el.style.setProperty("--my", `${e.clientY - r.top}px`);
}

export function Features() {
  const { t } = useI18n();
  return (
    <Section id="features" kicker={t.features.kicker} title={t.features.title} lead={t.features.lead}>
      <motion.div
        className="feature-grid"
        variants={stagger(0.08)}
        initial="hidden"
        whileInView="show"
        viewport={{ once: true, margin: "-60px" }}
      >
        {t.features.items.map((f) => (
          <motion.article
            key={f.icon}
            className="feature"
            variants={fadeUp}
            whileHover={{ y: -6 }}
            transition={{ type: "spring", stiffness: 300, damping: 24 }}
            onPointerMove={trackPointer}
          >
            <div className="icon">
              <FeatureIcon name={f.icon} />
            </div>
            <h3>{f.title}</h3>
            <p>{f.body}</p>
            <span className="tag">{f.tag}</span>
          </motion.article>
        ))}
      </motion.div>
    </Section>
  );
}
