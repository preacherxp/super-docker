const motion = window.matchMedia("(prefers-reduced-motion: reduce)");
const narrow = window.matchMedia("(max-width: 700px)");

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

const workflow = document.querySelector<HTMLElement>(".workflow-section")!;
const steps = [...document.querySelectorAll(".workflow-step")];
const key = document.querySelector<HTMLElement>("#hero-key")!;
const keyCaption = document.querySelector<HTMLElement>("#key-caption")!;
const keyFrames = [
  ["/", "A little less searching. A lot more finding."],
  ["e", "Right into your container. Right in your flow."],
  ["y", "Grab what you need. Keep on building."],
];
let currentStep = -1;
let framePending = false;
function updateScroll() {
  framePending = false;
  if (motion.matches || narrow.matches) return;
  const bounds = workflow.getBoundingClientRect();
  const progress = Math.max(
    0,
    Math.min(
      1,
      (83 - bounds.top) / Math.max(1, bounds.height - window.innerHeight + 83),
    ),
  );
  const step = Math.min(2, Math.floor(progress * 3));
  if (step !== currentStep) {
    currentStep = step;
    steps.forEach((element, index) =>
      element.classList.toggle("active", step === index),
    );
    key.textContent = keyFrames[step][0];
    keyCaption.textContent = keyFrames[step][1];
    key.classList.toggle("changed", step === 1);
  }
  if (window.scrollY < 1500)
    document
      .querySelector<HTMLElement>(".terminal")
      ?.style.setProperty(
        "--terminal-tilt",
        `${Math.max(0, 5 - window.scrollY / 65)}deg`,
      );
}
function scheduleScroll() {
  if (!framePending) {
    framePending = true;
    requestAnimationFrame(updateScroll);
  }
}
window.addEventListener("scroll", scheduleScroll, { passive: true });
window.addEventListener("resize", scheduleScroll);
motion.addEventListener("change", () => {
  if (motion.matches)
    document.documentElement.classList.remove("motion-enabled");
  scheduleScroll();
});
updateScroll();
