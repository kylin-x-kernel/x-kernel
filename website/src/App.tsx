import { MotionConfig } from "framer-motion";
import { Backdrop } from "./components/Backdrop";
import { Nav } from "./components/Nav";
import { Hero } from "./components/Hero";
import { Features } from "./components/Features";
import { Architecture } from "./components/Architecture";
import { Platforms } from "./components/Platforms";
import { Workloads } from "./components/Workloads";
import { Tee } from "./components/Tee";
import { Capabilities } from "./components/Capabilities";
import { Verification } from "./components/Verification";
import { QuickStart } from "./components/QuickStart";
import { Footer } from "./components/Footer";

export function App() {
  return (
    <MotionConfig reducedMotion="user">
      <Backdrop />
      <Nav />
      <main>
        <Hero />
        <Features />
        <Architecture />
        <Platforms />
        <Workloads />
        <Tee />
        <Capabilities />
        <Verification />
        <QuickStart />
      </main>
      <Footer />
    </MotionConfig>
  );
}
