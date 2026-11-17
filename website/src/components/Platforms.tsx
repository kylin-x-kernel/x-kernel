import type { CSSProperties } from "react";
import { motion } from "framer-motion";
import { useI18n } from "../i18n";
import { shared } from "../content";
import { Section, stagger } from "./Section";

export function Platforms() {
  const { t } = useI18n();
  const doubled = [...shared.platforms, ...shared.platforms];

  return (
    <Section id="platforms" kicker={t.platforms.kicker} title={t.platforms.title}>
      <motion.div
        className="arch-cards"
        variants={stagger(0.1)}
        initial="hidden"
        whileInView="show"
        viewport={{ once: true, margin: "-60px" }}
      >
        {shared.archs.map((a, i) => (
          <motion.div
            key={a.id}
            className="chip-card"
            style={{ "--c": a.color } as CSSProperties}
            variants={{
              hidden: { opacity: 0, y: 40, rotate: -2 },
              show: { opacity: 1, y: 0, rotate: 0, transition: { type: "spring", stiffness: 140, damping: 18 } },
            }}
            whileHover={{ y: -8, borderColor: a.color }}
          >
            {a.ci && <span className="ci-badge">● {t.platforms.ci}</span>}
            <div className="die">
              <span className="orbit" style={{ animationDuration: `${3 + i}s` }} />
              <span className="pins" />
              <span className="core">{a.id === "loongarch64" ? "LA64" : a.id === "riscv64" ? "RV64" : a.id === "aarch64" ? "A64" : "x64"}</span>
            </div>
            <h3>{a.name}</h3>
            <span className="isa">{a.isa}</span>
          </motion.div>
        ))}
      </motion.div>

      <div className="marquee" aria-label={t.platforms.targets}>
        <div className="marquee-track">
          {doubled.map((p, i) => (
            <span className="plat-pill" key={`${p}-${i}`} aria-hidden={i >= shared.platforms.length}>
              {p}
            </span>
          ))}
        </div>
      </div>
    </Section>
  );
}
