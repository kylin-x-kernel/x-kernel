import type { ReactNode } from "react";
import { motion, type Variants } from "framer-motion";

export const fadeUp: Variants = {
  hidden: { opacity: 0, y: 28, filter: "blur(6px)" },
  show: { opacity: 1, y: 0, filter: "blur(0px)", transition: { duration: 0.7, ease: [0.22, 1, 0.36, 1] } },
};

export const stagger = (gap = 0.08, delay = 0): Variants => ({
  hidden: {},
  show: { transition: { staggerChildren: gap, delayChildren: delay } },
});

interface SectionProps {
  id: string;
  kicker: string;
  title: ReactNode;
  lead?: ReactNode;
  children: ReactNode;
}

export function Section({ id, kicker, title, lead, children }: SectionProps) {
  return (
    <section id={id} className="section">
      <div className="container">
        <motion.header
          variants={stagger(0.1)}
          initial="hidden"
          whileInView="show"
          viewport={{ once: true, margin: "-80px" }}
        >
          <motion.span className="kicker" variants={fadeUp}>
            {kicker}
          </motion.span>
          <motion.h2 className="h2" variants={fadeUp}>
            {title}
          </motion.h2>
          {lead && (
            <motion.p className="lead" variants={fadeUp}>
              {lead}
            </motion.p>
          )}
        </motion.header>
        {children}
      </div>
    </section>
  );
}
