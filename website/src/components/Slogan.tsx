import { forwardRef, useEffect, useRef, useState } from "react";
import { AnimatePresence, LayoutGroup, motion, type Variants } from "framer-motion";
import { useI18n } from "../i18n";
import { shared } from "../content";

const ease = [0.22, 1, 0.36, 1] as const;

// Burst intervals decelerate like a slot machine reel: fast first, slowing into the stop.
const BURST_FIRST_MS = 45;
const BURST_LAST_MS = 120;
const LAND_MS = 1600;

function burstIntervalMs(i: number, n: number): number {
  const t = n <= 1 ? 1 : i / (n - 1);
  return Math.round(BURST_FIRST_MS + (BURST_LAST_MS - BURST_FIRST_MS) * t * t);
}

/** fast: a word flashing past mid-burst; land: the word a burst stops on; normal: everything else. */
type Speed = "fast" | "land" | "normal";

type Step = { phase: "build"; word: string; speed: Speed; holdMs: number } | { phase: "kernel"; holdMs: number };

type Group = { readonly burst: readonly string[]; readonly land: string };

function buildSteps(groups: readonly Group[]): Step[] {
  const steps: Step[] = [{ phase: "build", word: "X", speed: "normal", holdMs: 2000 }];
  for (const g of groups) {
    g.burst.forEach((w, i) => {
      steps.push({ phase: "build", word: w, speed: "fast", holdMs: burstIntervalMs(i, g.burst.length) });
    });
    steps.push({ phase: "build", word: g.land, speed: "land", holdMs: LAND_MS });
  }
  steps.push({ phase: "build", word: "X", speed: "normal", holdMs: 1000 });
  steps.push({ phase: "kernel", holdMs: 4200 });
  return steps;
}

/**
 * Whole-word flip: the old word rolls up and out while the next rolls in from below.
 * `custom` is the incoming word's speed, so the outgoing word also hurries during a burst.
 */
const flip: Variants = {
  // Fast words are near-instant swaps with a little vertical smear, reading as motion blur.
  enter: (sp: Speed) =>
    sp === "fast"
      ? { y: "22%", rotateX: 0, scale: 1, opacity: 0.35, filter: "blur(3px)" }
      : sp === "land"
        ? { y: "30%", rotateX: -25, scale: 1, opacity: 0.4, filter: "blur(4px)" }
        : { y: "90%", rotateX: -70, scale: 1, opacity: 0, filter: "blur(10px)" },
  center: (sp: Speed) => ({
    y: "0%",
    rotateX: 0,
    opacity: 1,
    filter: "blur(0px)",
    // Landing word overshoots slightly so the stop reads as a deliberate "lock".
    scale: sp === "land" ? [1, 1.07, 1] : 1,
    transition:
      sp === "fast"
        ? { duration: 0.04, ease: "linear" }
        : sp === "land"
          ? { duration: 0.32, ease, scale: { duration: 0.45, times: [0, 0.4, 1], delay: 0.05 } }
          : { duration: 0.6, ease },
  }),
  exit: (sp: Speed) =>
    sp === "normal"
      ? { y: "-90%", rotateX: 70, opacity: 0, filter: "blur(8px)", transition: { duration: 0.4, ease: "easeIn" } }
      : { opacity: 0, transition: { duration: 0 } },
};

/** The X glyph, shared across both slogan states via layoutId so it flies between lines. */
type XEntry = "intro" | "flip" | "fly";

const xVariants: Variants = {
  intro: { opacity: 0, scale: 2.4, filter: "blur(14px)" },
  enter: { y: "90%", rotateX: -70, opacity: 0, filter: "blur(10px)" },
  center: (entry: XEntry) => ({
    opacity: 1,
    scale: 1,
    y: "0%",
    rotateX: 0,
    filter: "blur(0px)",
    transition: { duration: entry === "intro" ? 1.1 : 0.6, ease },
  }),
  // The X only ever leaves into a burst, so it snaps out rather than flipping.
  exit: { opacity: 0, filter: "blur(4px)", transition: { duration: 0.05 } },
};

// forwardRef: AnimatePresence's popLayout mode measures its direct children through a ref.
const XGlyph = forwardRef<HTMLSpanElement, { entry: XEntry }>(function XGlyph({ entry }, ref) {
  return (
    <motion.span
      ref={ref}
      layoutId="hero-x"
      className="xglyph is-x"
      data-text="X"
      custom={entry}
      variants={xVariants}
      initial={entry === "fly" ? false : entry === "intro" ? "intro" : "enter"}
      animate="center"
      exit="exit"
      transition={{ layout: { type: "spring", stiffness: 140, damping: 20 } }}
    >
      X
    </motion.span>
  );
});

export function Slogan() {
  const { t, lang } = useI18n();
  const s = t.hero.slogan;
  const steps = useRef(buildSteps(shared.sloganGroups)).current;
  const [stepIdx, setStepIdx] = useState(0);
  const isIntroRef = useRef(true);
  const isReducedMotion = useRef(
    typeof window !== "undefined" && window.matchMedia("(prefers-reduced-motion: reduce)").matches,
  ).current;

  useEffect(() => {
    if (isReducedMotion) return;
    const id = window.setTimeout(() => {
      isIntroRef.current = false;
      setStepIdx((i) => (i + 1) % steps.length);
    }, steps[stepIdx].holdMs);
    return () => window.clearTimeout(id);
  }, [stepIdx, steps, isReducedMotion]);

  const step: Step = isReducedMotion ? { phase: "kernel", holdMs: 0 } : steps[stepIdx];
  const isKernel = step.phase === "kernel";
  const word = step.phase === "build" ? step.word : "X";
  const speed: Speed = step.phase === "build" ? step.speed : "normal";
  const isXShown = word === "X";
  // The X flies when it changes lines (to or from "X-Kernel"); between words it flips like one.
  const prevPhaseRef = useRef(step.phase);
  const xEntry: XEntry = isIntroRef.current ? "intro" : prevPhaseRef.current === "kernel" ? "fly" : "flip";
  useEffect(() => {
    prevPhaseRef.current = step.phase;
  });

  return (
    <h1 className="slogan" aria-label={`Build the X — X-Kernel, ${s.tagline}`}>
      <LayoutGroup>
        <span className="slogan-line" aria-hidden>
          <AnimatePresence mode="popLayout" initial={false}>
            {isKernel ? (
              <motion.span key="kernel" className="slogan-row">
                <XGlyph entry="fly" />
                <span className="kernel-suffix">
                  {"-Kernel".split("").map((ch, i) => (
                    <motion.span
                      key={i}
                      initial={{ opacity: 0, x: -24, filter: "blur(8px)" }}
                      animate={{ opacity: 1, x: 0, filter: "blur(0px)" }}
                      exit={{ opacity: 0, x: -16, filter: "blur(6px)", transition: { duration: 0.25, delay: (6 - i) * 0.025 } }}
                      transition={{ duration: 0.5, delay: 0.35 + i * 0.05, ease }}
                    >
                      {ch}
                    </motion.span>
                  ))}
                </span>
              </motion.span>
            ) : (
              <motion.span
                key="build"
                className="slogan-row"
                initial={{ opacity: 0, y: 20, filter: "blur(8px)" }}
                animate={{ opacity: 1, y: 0, filter: "blur(0px)" }}
                // Leave fast and undelayed so the flying X lands on an empty line.
                exit={{ opacity: 0, y: -20, filter: "blur(8px)", transition: { duration: 0.25 } }}
                transition={{ duration: 0.55, ease, delay: isIntroRef.current ? 0.3 : 0.25 }}
              >
                {s.build}
              </motion.span>
            )}
          </AnimatePresence>
        </span>

        <span className="slogan-line" aria-hidden>
          <AnimatePresence mode="popLayout" initial={false}>
            {isKernel ? (
              <motion.span
                key="tagline"
                className={`slogan-tagline grad-text ${lang === "zh" ? "zh" : ""}`}
                // English is long, so it barely spreads; it would otherwise brush the terminal.
                initial={{ opacity: 0, y: 24, letterSpacing: lang === "zh" ? "0.3em" : "0.04em" }}
                animate={{ opacity: 1, y: 0, letterSpacing: lang === "zh" ? "0.06em" : "-0.02em" }}
                exit={{ opacity: 0, y: 16, transition: { duration: 0.3 } }}
                transition={{ duration: 0.8, delay: 0.55, ease }}
              >
                {s.tagline}
              </motion.span>
            ) : (
              <motion.span key="words" className="slogan-row word-row">
                <span className="word-slot">
                  <AnimatePresence mode="popLayout" initial={false} custom={speed}>
                    {isXShown ? (
                      <XGlyph key="x" entry={xEntry} />
                    ) : (
                      <motion.span
                        key={`${stepIdx}-${word}`}
                        className="xglyph is-word"
                        data-text={word}
                        custom={speed}
                        variants={flip}
                        initial="enter"
                        animate="center"
                        exit="exit"
                      >
                        {word}
                      </motion.span>
                    )}
                  </AnimatePresence>
                </span>
                <motion.span className="slogan-dot" layout transition={{ type: "spring", stiffness: 520, damping: 38 }}>
                  .
                </motion.span>
              </motion.span>
            )}
          </AnimatePresence>
        </span>
      </LayoutGroup>
    </h1>
  );
}
