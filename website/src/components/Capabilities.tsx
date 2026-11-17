import { motion } from "framer-motion";
import { useI18n } from "../i18n";
import { Section, fadeUp, stagger } from "./Section";

export function Capabilities() {
  const { t } = useI18n();
  const c = t.capabilities;

  return (
    <Section id="capabilities" kicker={c.kicker} title={c.title} lead={c.lead}>
      <div className="cap-legend">
        <span>
          <i className="led implemented" style={{ width: 8, height: 8, borderRadius: "50%", display: "inline-block" }} />
          {c.implemented}
        </span>
        <span>
          <i className="led partial" style={{ width: 8, height: 8, borderRadius: "50%", display: "inline-block" }} />
          {c.partial}
        </span>
      </div>
      <motion.div
        className="cap-grid"
        variants={stagger(0.05)}
        initial="hidden"
        whileInView="show"
        viewport={{ once: true, margin: "-60px" }}
      >
        {c.items.map((item) => {
          const isDone = item.status === "implemented";
          return (
            <motion.div className="cap" key={item.area} variants={fadeUp}>
              <span className={`led ${item.status}`} aria-label={isDone ? c.implemented : c.partial} />
              <h4>{item.area}</h4>
              <span className="src">{item.source}</span>
              <p>{item.scope}</p>
              {/* Status band, not a percentage: full for implemented, half for partial. */}
              <span className="meter" aria-hidden>
                <motion.i
                  style={{ background: isDone ? "var(--green)" : "var(--amber)" }}
                  initial={{ scaleX: 0 }}
                  whileInView={{ scaleX: isDone ? 1 : 0.5 }}
                  viewport={{ once: true }}
                  transition={{ duration: 1.2, ease: [0.22, 1, 0.36, 1], delay: 0.2 }}
                />
              </span>
            </motion.div>
          );
        })}
      </motion.div>
    </Section>
  );
}
