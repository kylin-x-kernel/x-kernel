import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { shared } from "../content";

const LINE_DELAY_MS = 420;
const HOLD_MS = 4200;

/** Stylized boot sequence that loops; an illustration, not captured output. */
export function BootTerminal() {
  const lines = shared.bootLog;
  const [count, setCount] = useState(0);
  const [cycle, setCycle] = useState(0);

  useEffect(() => {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      setCount(lines.length);
      return;
    }
    if (count < lines.length) {
      const id = window.setTimeout(() => setCount((c) => c + 1), count === 0 ? 700 : LINE_DELAY_MS);
      return () => window.clearTimeout(id);
    }
    const id = window.setTimeout(() => {
      setCount(0);
      setCycle((c) => c + 1);
    }, HOLD_MS);
    return () => window.clearTimeout(id);
  }, [count, lines.length]);

  return (
    <div className="term">
      <div className="term-bar">
        <i />
        <i />
        <i />
        <span>qemu-system-aarch64 — x-kernel</span>
      </div>
      <div className="term-body" aria-live="off">
        <AnimatePresence mode="popLayout">
          {lines.slice(0, count).map((l, i) => (
            <motion.div
              key={`${cycle}-${i}`}
              className="term-line"
              initial={{ opacity: 0, x: -10 }}
              animate={{ opacity: 1, x: 0 }}
              exit={{ opacity: 0 }}
              transition={{ duration: 0.3 }}
            >
              <span className="ts">[{l.t.padStart(11, " ")}]</span>
              <span className="tag">{l.tag}</span>
              <span className="ok">✓</span>
              <span className="msg">{l.msg}</span>
            </motion.div>
          ))}
        </AnimatePresence>
        {count >= lines.length ? (
          <div className="term-line">
            <span className="prompt">root@x-kernel:~#</span>
            <span className="caret" />
          </div>
        ) : (
          <span className="caret" />
        )}
      </div>
      <div className="term-scan" />
    </div>
  );
}
