import { useEffect, useRef, useState } from "react";
import { animate, useInView } from "framer-motion";

export function Counter({ value, suffix = "", delay = 0 }: { value: number; suffix?: string; delay?: number }) {
  const ref = useRef<HTMLElement>(null);
  const isInView = useInView(ref, { once: true });
  const [shown, setShown] = useState(0);

  useEffect(() => {
    if (!isInView) return;
    const controls = animate(0, value, {
      duration: 1.8,
      delay,
      ease: [0.16, 1, 0.3, 1],
      onUpdate: (v) => setShown(Math.round(v)),
    });
    return () => controls.stop();
  }, [isInView, value, delay]);

  return (
    <b ref={ref}>
      {shown}
      <span className="grad-text" style={{ fontSize: "inherit", color: "transparent" }}>
        {suffix}
      </span>
    </b>
  );
}
