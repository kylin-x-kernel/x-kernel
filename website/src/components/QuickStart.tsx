import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { useI18n } from "../i18n";
import { shared } from "../content";
import { fadeUp, stagger } from "./Section";

export function QuickStart() {
  const { t } = useI18n();
  const [activeId, setActiveId] = useState<string>(shared.quickstart[0].id);
  const [isCopied, setIsCopied] = useState(false);
  const active = shared.quickstart.find((q) => q.id === activeId) ?? shared.quickstart[0];

  useEffect(() => {
    if (!isCopied) return;
    const id = window.setTimeout(() => setIsCopied(false), 1600);
    return () => window.clearTimeout(id);
  }, [isCopied]);

  const copy = async () => {
    const text = active.lines.map((l) => l.replace(/\s+#.*$/, "")).join("\n");
    try {
      await navigator.clipboard.writeText(text);
      setIsCopied(true);
    } catch {
      // Clipboard may be unavailable (insecure context); ignore.
    }
  };

  return (
    <section id="start" className="section">
      <div className="container qs">
        <motion.div variants={stagger(0.1)} initial="hidden" whileInView="show" viewport={{ once: true, margin: "-80px" }}>
          <motion.span className="kicker" variants={fadeUp}>
            {t.start.kicker}
          </motion.span>
          <motion.h2 className="h2" variants={fadeUp}>
            {t.start.title}
          </motion.h2>
          <motion.p className="lead" variants={fadeUp}>
            {t.start.lead}
          </motion.p>
          <motion.div className="tabs" variants={fadeUp} role="tablist">
            {shared.quickstart.map((q) => (
              <button
                key={q.id}
                role="tab"
                aria-selected={q.id === activeId}
                className={`tab ${q.id === activeId ? "on" : ""}`}
                onClick={() => setActiveId(q.id)}
              >
                {q.id === activeId && <motion.span layoutId="qs-tab" className="tab-bg" transition={{ type: "spring", stiffness: 400, damping: 32 }} />}
                <span>{q.label}</span>
              </button>
            ))}
          </motion.div>
        </motion.div>

        <motion.div
          className="term"
          initial={{ opacity: 0, x: 40 }}
          whileInView={{ opacity: 1, x: 0 }}
          viewport={{ once: true, margin: "-80px" }}
          transition={{ duration: 0.8, ease: [0.22, 1, 0.36, 1] }}
        >
          <div className="term-bar">
            <i />
            <i />
            <i />
            <span>~/x-kernel</span>
            <button className="copy" onClick={copy}>
              {isCopied ? `✓ ${t.start.copied}` : t.start.copy}
            </button>
          </div>
          <div className="term-body">
            <AnimatePresence mode="wait">
              <motion.div key={active.id} initial="hidden" animate="show" exit={{ opacity: 0 }} variants={stagger(0.12)}>
                {active.lines.map((line) => {
                  const hashAt = line.indexOf("#");
                  const cmd = hashAt >= 0 ? line.slice(0, hashAt) : line;
                  const comment = hashAt >= 0 ? line.slice(hashAt) : "";
                  return (
                    <motion.div
                      key={line}
                      className="qs-line"
                      variants={{ hidden: { opacity: 0, x: -12 }, show: { opacity: 1, x: 0 } }}
                    >
                      <span className="p">{line.startsWith(" ") ? " " : "$"}</span>
                      <span>
                        {cmd}
                        <span className="cm">{comment}</span>
                      </span>
                    </motion.div>
                  );
                })}
                <div className="qs-line">
                  <span className="p">$</span>
                  <span className="caret" />
                </div>
              </motion.div>
            </AnimatePresence>
          </div>
          <div className="term-scan" />
        </motion.div>
      </div>
    </section>
  );
}
