import { useEffect } from "react";
import { motion, useMotionValue, useScroll, useSpring } from "framer-motion";

export function Backdrop() {
  const { scrollYProgress } = useScroll();
  const progress = useSpring(scrollYProgress, { stiffness: 120, damping: 30, mass: 0.3 });

  const x = useMotionValue(-1000);
  const y = useMotionValue(-1000);
  const sx = useSpring(x, { stiffness: 80, damping: 20 });
  const sy = useSpring(y, { stiffness: 80, damping: 20 });

  useEffect(() => {
    const onMove = (e: PointerEvent) => {
      x.set(e.clientX);
      y.set(e.clientY);
    };
    window.addEventListener("pointermove", onMove, { passive: true });
    return () => window.removeEventListener("pointermove", onMove);
  }, [x, y]);

  return (
    <>
      <div className="backdrop" aria-hidden />
      <motion.div className="cursor-glow" style={{ x: sx, y: sy }} aria-hidden />
      <motion.div className="progress" style={{ scaleX: progress }} aria-hidden />
    </>
  );
}
