import { useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { useI18n } from "../i18n";
import { Section } from "./Section";

function Whale() {
  return (
    <svg className="whale" viewBox="0 0 200 120" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinejoin="round" aria-hidden>
      {[0, 1, 2, 3].map((c) => (
        <rect key={`a${c}`} x={46 + c * 22} y={36} width={18} height={16} rx={2} />
      ))}
      {[0, 1, 2].map((c) => (
        <rect key={`b${c}`} x={68 + c * 22} y={16} width={18} height={16} rx={2} />
      ))}
      <path d="M20 58h160l-18 34H38z" />
      <path d="M10 104c15 0 15-6 30-6s15 6 30 6 15-6 30-6 15 6 30 6 15-6 30-6 15 6 30 6" opacity="0.5" />
    </svg>
  );
}

export function Workloads() {
  const { t } = useI18n();
  const items = t.workloads.items;
  const [activeId, setActiveId] = useState(items[2].id);
  const active = items.find((w) => w.id === activeId) ?? items[0];

  return (
    <Section id="workloads" kicker={t.workloads.kicker} title={t.workloads.title} lead={t.workloads.lead}>
      <div className="tabs" role="tablist">
        {items.map((w) => (
          <button
            key={w.id}
            role="tab"
            aria-selected={w.id === activeId}
            className={`tab ${w.id === activeId ? "on" : ""}`}
            onClick={() => setActiveId(w.id)}
          >
            {w.id === activeId && <motion.span layoutId="wl-tab" className="tab-bg" transition={{ type: "spring", stiffness: 400, damping: 32 }} />}
            <span>{w.label}</span>
          </button>
        ))}
      </div>

      <AnimatePresence mode="wait">
        <motion.div
          key={active.id}
          className="workload"
          initial={{ opacity: 0, y: 24 }}
          animate={{ opacity: 1, y: 0 }}
          exit={{ opacity: 0, y: -16 }}
          transition={{ duration: 0.45, ease: [0.22, 1, 0.36, 1] }}
        >
          <div>
            <h3>{active.title}</h3>
            <p>{active.body}</p>
            <div className="term">
              <div className="term-bar">
                <i />
                <i />
                <i />
                <span>{active.id}</span>
              </div>
              <div className="term-body">
                <div className="term-line" style={{ whiteSpace: "normal" }}>
                  <span className="prompt">$</span>
                  <span style={{ color: "#e2e8f0" }}>{active.command}</span>
                </div>
                {active.output.map((line, i) => (
                  <motion.div
                    key={i}
                    className="term-line"
                    style={{ whiteSpace: "normal", color: line.startsWith("Hello") ? "var(--cyan)" : "#94a3b8" }}
                    initial={{ opacity: 0 }}
                    animate={{ opacity: 1 }}
                    transition={{ delay: 0.3 + i * 0.18 }}
                  >
                    {line || " "}
                  </motion.div>
                ))}
              </div>
            </div>
          </div>

          {active.image ? (
            <motion.figure
              className="shot"
              style={{ margin: 0 }}
              initial={{ opacity: 0, scale: 0.96, rotateX: 8 }}
              animate={{ opacity: 1, scale: 1, rotateX: 0 }}
              transition={{ duration: 0.7, ease: [0.22, 1, 0.36, 1] }}
            >
              <img src={active.image} alt={active.caption ?? active.title} loading="lazy" />
              {active.caption && <figcaption>{active.caption}</figcaption>}
            </motion.figure>
          ) : (
            <motion.div
              className="shot docker"
              initial={{ opacity: 0, scale: 0.96 }}
              animate={{ opacity: 1, scale: 1 }}
              transition={{ duration: 0.7 }}
            >
              <motion.div animate={{ y: [0, -10, 0], rotate: [0, -1.5, 0] }} transition={{ duration: 4, repeat: Infinity, ease: "easeInOut" }}>
                <Whale />
              </motion.div>
            </motion.div>
          )}
        </motion.div>
      </AnimatePresence>
    </Section>
  );
}
