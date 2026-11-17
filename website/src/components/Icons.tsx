import type { Feature } from "../content";

const common = {
  width: 24,
  height: 24,
  viewBox: "0 0 24 24",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.6,
  strokeLinecap: "round" as const,
  strokeLinejoin: "round" as const,
};

export function Logo() {
  return (
    <svg viewBox="0 0 64 64" aria-hidden>
      <defs>
        <linearGradient id="xk-logo" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#22d3ee" />
          <stop offset="1" stopColor="#a855f7" />
        </linearGradient>
      </defs>
      <path d="M32 4 56 18v28L32 60 8 46V18Z" fill="rgba(34,211,238,0.06)" stroke="url(#xk-logo)" strokeWidth="3" />
      <path d="M22 22 42 42M42 22 22 42" stroke="url(#xk-logo)" strokeWidth="5" strokeLinecap="round" />
    </svg>
  );
}

export function FeatureIcon({ name }: { name: Feature["icon"] }) {
  switch (name) {
    case "abi":
      return (
        <svg {...common}>
          <path d="M4 17l5-5-5-5M12 19h8" />
        </svg>
      );
    case "arch":
      return (
        <svg {...common}>
          <rect x="6" y="6" width="12" height="12" rx="2" />
          <rect x="9.5" y="9.5" width="5" height="5" rx="1" />
          <path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4" />
        </svg>
      );
    case "tee":
      return (
        <svg {...common}>
          <path d="M12 3l8 3v6c0 4.5-3.4 8.3-8 9-4.6-.7-8-4.5-8-9V6z" />
          <rect x="9" y="11" width="6" height="5" rx="1" />
          <path d="M10 11V9.5a2 2 0 014 0V11" />
        </svg>
      );
    case "gpu":
      return (
        <svg {...common}>
          <rect x="3" y="4" width="18" height="12" rx="2" />
          <path d="M8 20h8M12 16v4M7 12l3-3 2 2 4-4" />
        </svg>
      );
    case "crate":
      return (
        <svg {...common}>
          <path d="M12 2l9 5v10l-9 5-9-5V7z" />
          <path d="M12 12l9-5M12 12v10M12 12L3 7" />
        </svg>
      );
    case "verify":
      return (
        <svg {...common}>
          <path d="M9 12l2 2 4-4" />
          <circle cx="12" cy="12" r="9" />
        </svg>
      );
  }
}

export function ArrowIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" aria-hidden>
      <path d="M5 12h14M13 6l6 6-6 6" />
    </svg>
  );
}

export function GitIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" aria-hidden>
      <circle cx="6" cy="6" r="2.5" />
      <circle cx="6" cy="18" r="2.5" />
      <circle cx="18" cy="9" r="2.5" />
      <path d="M6 8.5v7M18 11.5c0 3-3 4-9.5 5" />
    </svg>
  );
}
