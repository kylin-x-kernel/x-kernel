import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { useI18n } from "../i18n";
import { shared, type Lang } from "../content";
import { GitIcon, Logo } from "./Icons";

const SECTIONS = ["features", "architecture", "workloads", "tee", "capabilities", "start"] as const;

export function Nav() {
  const { t, lang, setLang } = useI18n();
  const [isScrolled, setIsScrolled] = useState(false);
  const [active, setActive] = useState<string>("");
  const [isMenuOpen, setIsMenuOpen] = useState(false);

  useEffect(() => {
    const onScroll = () => setIsScrolled(window.scrollY > 24);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, []);

  useEffect(() => {
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) setActive(entry.target.id);
        }
      },
      { rootMargin: "-45% 0px -50% 0px" },
    );
    for (const id of SECTIONS) {
      const el = document.getElementById(id);
      if (el) observer.observe(el);
    }
    return () => observer.disconnect();
  }, []);

  const langs: Lang[] = ["en", "zh"];

  return (
    <>
      <motion.nav
        className={`nav ${isScrolled ? "scrolled" : ""}`}
        initial={{ y: -80, opacity: 0 }}
        animate={{ y: 0, opacity: 1 }}
        transition={{ duration: 0.6, ease: [0.22, 1, 0.36, 1] }}
      >
        <div className="container nav-inner">
          <a href="#top" className="brand" aria-label="X-Kernel home">
            <Logo />
            X-Kernel
          </a>
          <div className="nav-links">
            {SECTIONS.map((id) => (
              <a key={id} href={`#${id}`} className={active === id ? "active" : ""}>
                {active === id && (
                  <motion.span layoutId="nav-pill" className="pill" transition={{ type: "spring", stiffness: 400, damping: 34 }} />
                )}
                {t.nav[id]}
              </a>
            ))}
          </div>
          <div className="nav-actions">
            <div className="lang-toggle" role="group" aria-label="Language">
              {langs.map((l) => (
                <button key={l} className={lang === l ? "on" : ""} onClick={() => setLang(l)} aria-pressed={lang === l}>
                  {lang === l && <motion.span layoutId="lang-knob" className="knob" transition={{ type: "spring", stiffness: 500, damping: 36 }} />}
                  {l === "en" ? "EN" : "中"}
                </button>
              ))}
            </div>
            <a className="btn small" href={shared.links.repo} target="_blank" rel="noreferrer">
              <GitIcon />
              {t.nav.repo}
            </a>
            <button className="menu-btn" aria-label="Menu" aria-expanded={isMenuOpen} onClick={() => setIsMenuOpen((v) => !v)}>
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
                {isMenuOpen ? <path d="M6 6l12 12M18 6L6 18" /> : <path d="M4 7h16M4 12h16M4 17h16" />}
              </svg>
            </button>
          </div>
        </div>
      </motion.nav>
      <AnimatePresence>
        {isMenuOpen && (
          <motion.div
            className="mobile-menu"
            initial={{ opacity: 0, y: -12 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -12 }}
            transition={{ duration: 0.2 }}
          >
            {SECTIONS.map((id) => (
              <a key={id} href={`#${id}`} onClick={() => setIsMenuOpen(false)}>
                {t.nav[id]}
              </a>
            ))}
            <a href={shared.links.repo} target="_blank" rel="noreferrer">
              {t.nav.repo} ↗
            </a>
          </motion.div>
        )}
      </AnimatePresence>
    </>
  );
}
