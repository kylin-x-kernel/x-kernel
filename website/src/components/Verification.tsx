import { motion } from "framer-motion";
import { useI18n } from "../i18n";
import { Section, fadeUp, stagger } from "./Section";

/** Illustrative snippet showing the project's SAFETY: convention. */
function SafetySnippet() {
  return (
    <pre>
      <code>
        <span className="tok-k">pub fn</span> <span className="tok-f">read_reg</span>(&amp;
        <span className="tok-k">self</span>, offset: <span className="tok-t">usize</span>) -&gt; <span className="tok-t">u32</span> {"{"}
        {"\n"}
        {"    "}
        <span className="tok-k">debug_assert!</span>(offset &lt; <span className="tok-k">self</span>.len);
        {"\n"}
        {"    "}
        <span className="tok-c">
          // <span className="tok-s">SAFETY:</span> `base` maps this device's MMIO window for
        </span>
        {"\n"}
        {"    "}
        <span className="tok-c">// the lifetime of `self`, and `offset` is bounds-checked</span>
        {"\n"}
        {"    "}
        <span className="tok-c">// and 4-byte aligned by the register map.</span>
        {"\n"}
        {"    "}
        <span className="tok-k">unsafe</span> {"{ "}
        core::ptr::<span className="tok-f">read_volatile</span>(<span className="tok-k">self</span>.base.<span className="tok-f">add</span>(offset).<span className="tok-f">cast</span>()){" }"}
        {"\n"}
        {"}"}
      </code>
    </pre>
  );
}

export function Verification() {
  const { t } = useI18n();
  const v = t.verify;

  return (
    <Section id="verify" kicker={v.kicker} title={v.title} lead={v.lead}>
      <div className="verify-grid">
        <motion.div
          className="term code-card"
          initial={{ opacity: 0, y: 30 }}
          whileInView={{ opacity: 1, y: 0 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.8, ease: [0.22, 1, 0.36, 1] }}
        >
          <div className="term-bar">
            <i />
            <i />
            <i />
            <span>drivers/…/mmio.rs</span>
          </div>
          <SafetySnippet />
          <div className="term-scan" />
        </motion.div>

        <motion.div
          className="vlayers"
          variants={stagger(0.12)}
          initial="hidden"
          whileInView="show"
          viewport={{ once: true, margin: "-60px" }}
        >
          {v.layers.map((l, i) => (
            <motion.div className="vlayer" key={l.name} variants={fadeUp}>
              <span className="num">{i + 1}</span>
              <h4>{l.name}</h4>
              <code>{l.cmd}</code>
              <p>{l.body}</p>
            </motion.div>
          ))}
        </motion.div>
      </div>
    </Section>
  );
}
