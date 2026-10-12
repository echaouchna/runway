// Card spotlight and reveal for the documentation. Material swaps pages
// without reloading (navigation.instant), so this runs again on each page.
(() => {
  "use strict";
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;

  const setup = () => {
    const cards = document.querySelectorAll(".md-typeset .grid.cards > ul > li");
    cards.forEach((card, i) => {
      card.addEventListener("pointermove", (e) => {
        const r = card.getBoundingClientRect();
        card.style.setProperty("--mx", `${e.clientX - r.left}px`);
        card.style.setProperty("--my", `${e.clientY - r.top}px`);
      });
      card.style.setProperty("--i", String(i % 6));
    });
    if (reduce || !("IntersectionObserver" in window)) return;
    const io = new IntersectionObserver(
      (entries) => entries.forEach((e) => {
        if (!e.isIntersecting) return;
        e.target.classList.add("rw-in");
        io.unobserve(e.target);
      }),
      { threshold: 0.1 },
    );
    cards.forEach((card) => {
      card.classList.add("rw-reveal");
      io.observe(card);
    });
  };

  if (window.document$ && typeof window.document$.subscribe === "function") {
    window.document$.subscribe(setup);
  } else if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", setup);
  } else {
    setup();
  }
})();
