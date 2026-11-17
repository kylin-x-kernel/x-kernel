import { motion } from "framer-motion";
import { useI18n } from "../i18n";
import { shared } from "../content";
import { ArrowIcon, GitIcon, Logo } from "./Icons";

export function Footer() {
  const { t } = useI18n();
  const f = t.footer;
  const L = shared.links;

  return (
    <>
      <div className="container">
        <motion.div
          className="cta-band"
          initial={{ opacity: 0, y: 40, scale: 0.98 }}
          whileInView={{ opacity: 1, y: 0, scale: 1 }}
          viewport={{ once: true, margin: "-60px" }}
          transition={{ duration: 0.8, ease: [0.22, 1, 0.36, 1] }}
        >
          <h2 className="h2">
            Build the <span className="grad-text">X</span>.<br />
            <span className="footer-tagline">X-Kernel · {t.hero.slogan.tagline}</span>
          </h2>
          <div className="cta-row">
            <a className="btn primary" href={L.repo} target="_blank" rel="noreferrer">
              <GitIcon />
              {t.hero.ctaSecondary}
            </a>
            <a className="btn" href={L.chat} target="_blank" rel="noreferrer">
              {f.chat}
              <ArrowIcon />
            </a>
          </div>
        </motion.div>
      </div>
      <footer>
        <div className="container">
          <div className="foot">
            <div>
              <a href="#top" className="brand">
                <Logo />
                X-Kernel
              </a>
              <p>{f.tagline}</p>
            </div>
            <div>
              <h5>{f.community}</h5>
              <ul>
                <li><a href={L.repo} target="_blank" rel="noreferrer">{f.repo}</a></li>
                <li><a href={L.issues} target="_blank" rel="noreferrer">{f.issues}</a></li>
                <li><a href={L.pulls} target="_blank" rel="noreferrer">{f.pulls}</a></li>
                <li><a href={L.chat} target="_blank" rel="noreferrer">{f.chat}</a></li>
              </ul>
            </div>
            <div>
              <h5>{f.project}</h5>
              <ul>
                <li><a href={L.harness} target="_blank" rel="noreferrer">{f.harness}</a></li>
                <li><a href={L.agentos} target="_blank" rel="noreferrer">{f.agentos}</a></li>
                <li><a href="#capabilities">{t.nav.capabilities}</a></li>
                <li><a href="#start">{t.nav.start}</a></li>
              </ul>
            </div>
          </div>
          <div className="foot-bottom">
            <span>© {new Date().getFullYear()} X-Kernel · {f.license}</span>
            <span>
              {f.thanks} <a href={L.starry} target="_blank" rel="noreferrer">StarryOS</a> {f.and}{" "}
              <a href={L.arceos} target="_blank" rel="noreferrer">ArceOS</a>
            </span>
          </div>
        </div>
      </footer>
    </>
  );
}
