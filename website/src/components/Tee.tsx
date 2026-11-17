import { motion } from "framer-motion";
import { useI18n } from "../i18n";
import { shared } from "../content";
import { Section, fadeUp, stagger } from "./Section";

const RINGS = [
  { inset: "0%", dur: 60, dash: true, dir: 1 },
  { inset: "10%", dur: 40, dash: false, dir: -1 },
  { inset: "20%", dur: 28, dash: true, dir: 1 },
];

export function Tee() {
  const { t } = useI18n();
  const caps = t.tee.caps;

  return (
    <Section id="tee" kicker={t.tee.kicker} title={t.tee.title} lead={t.tee.lead}>
      <div className="tee-grid">
        <motion.div
          className="vault"
          initial={{ opacity: 0, scale: 0.85 }}
          whileInView={{ opacity: 1, scale: 1 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 1, ease: [0.22, 1, 0.36, 1] }}
        >
          {RINGS.map((r, i) => (
            <motion.span
              key={i}
              className={`ring ${r.dash ? "dash" : ""}`}
              style={{ inset: r.inset }}
              animate={{ rotate: 360 * r.dir }}
              transition={{ duration: r.dur, repeat: Infinity, ease: "linear" }}
            />
          ))}
          {/* Capability labels orbiting the enclave. */}
          <motion.div
            style={{ position: "absolute", inset: 0 }}
            animate={{ rotate: 360 }}
            transition={{ duration: 80, repeat: Infinity, ease: "linear" }}
          >
            {shared.teeOrbit.map((c, i) => {
              const angle = (i / shared.teeOrbit.length) * Math.PI * 2 - Math.PI / 2;
              const radius = i % 2 === 0 ? 42 : 33;
              return (
                <motion.span
                  key={c}
                  className="node"
                  style={{ left: `${50 + Math.cos(angle) * radius}%`, top: `${50 + Math.sin(angle) * radius}%` }}
                  animate={{ rotate: -360 }}
                  transition={{ duration: 80, repeat: Infinity, ease: "linear" }}
                >
                  {c}
                </motion.span>
              );
            })}
          </motion.div>
          <motion.div
            className="center"
            animate={{ boxShadow: ["0 0 60px -10px rgba(168,85,247,0.5)", "0 0 110px -6px rgba(168,85,247,0.9)", "0 0 60px -10px rgba(168,85,247,0.5)"] }}
            transition={{ duration: 3, repeat: Infinity, ease: "easeInOut" }}
          >
            <svg width="56" height="56" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
              <path d="M12 3l8 3v6c0 4.5-3.4 8.3-8 9-4.6-.7-8-4.5-8-9V6z" />
              <rect x="9" y="11" width="6" height="5" rx="1" />
              <path d="M10 11V9.5a2 2 0 014 0V11" />
            </svg>
          </motion.div>
        </motion.div>

        <motion.div
          className="tee-cards"
          variants={stagger(0.12)}
          initial="hidden"
          whileInView="show"
          viewport={{ once: true, margin: "-60px" }}
        >
          {t.tee.platforms.map((p) => (
            <motion.div className="tee-card" key={p.name} variants={fadeUp}>
              <h4>{p.name}</h4>
              <span className="st">{p.state}</span>
              <p>{p.body}</p>
            </motion.div>
          ))}
          <motion.div className="tee-caps" variants={fadeUp}>
            {caps.map((c) => (
              <span key={c}>{c}</span>
            ))}
          </motion.div>
        </motion.div>
      </div>
    </Section>
  );
}
