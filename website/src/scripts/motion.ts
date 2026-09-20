const motion = window.matchMedia("(prefers-reduced-motion: reduce)");

if (!motion.matches && "IntersectionObserver" in window) {
  document.documentElement.classList.add("motion-enabled");
  const reveals = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) {
          entry.target.classList.add("is-visible");
          reveals.unobserve(entry.target);
        }
      }
    },
    { threshold: 0.08 },
  );
  document
    .querySelectorAll(".reveal")
    .forEach((element) => reveals.observe(element));
}

motion.addEventListener("change", () => {
  if (motion.matches)
    document.documentElement.classList.remove("motion-enabled");
});
