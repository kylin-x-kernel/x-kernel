import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { content, type Content, type Lang } from "./content";

interface I18n {
  lang: Lang;
  t: Content;
  setLang: (lang: Lang) => void;
}

const I18nContext = createContext<I18n | null>(null);

const STORAGE_KEY = "xk-lang";

function initialLang(): Lang {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved === "en" || saved === "zh") return saved;
  } catch {
    // Storage can be blocked; fall back to the browser language.
  }
  return navigator.language.toLowerCase().startsWith("zh") ? "zh" : "en";
}

export function I18nProvider({ children }: { children: ReactNode }) {
  const [lang, setLang] = useState<Lang>(initialLang);

  useEffect(() => {
    document.documentElement.lang = lang === "zh" ? "zh-CN" : "en";
    try {
      localStorage.setItem(STORAGE_KEY, lang);
    } catch {
      // Non-essential preference.
    }
  }, [lang]);

  const value = useMemo(() => ({ lang, t: content[lang], setLang }), [lang]);
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18n {
  const ctx = useContext(I18nContext);
  if (!ctx) throw new Error("useI18n must be used inside I18nProvider");
  return ctx;
}
