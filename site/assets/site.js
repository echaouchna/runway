// runway.echaouchna.dev: progressive enhancement only. The page reads well
// without JavaScript; with it, things move (unless reduced motion is asked).
(() => {
  "use strict";
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => [...r.querySelectorAll(s)];
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const visible = (el, cb, opts = { threshold: 0.2 }) => {
    const io = new IntersectionObserver((es) => es.forEach((e) => cb(e.isIntersecting, e)), opts);
    io.observe(el);
    return io;
  };

  // ---------------------------------------------------------- reveal
  const revealer = new IntersectionObserver(
    (es) => es.forEach((e) => {
      if (e.isIntersecting) {
        e.target.classList.add("in");
        revealer.unobserve(e.target);
      }
    }),
    { threshold: 0.12, rootMargin: "0px 0px -6% 0px" },
  );
  $$("[data-reveal]").forEach((el) => (reduce ? el.classList.add("in") : revealer.observe(el)));

  // ---------------------------------------------------------- nav and progress
  const topbar = $(".topbar");
  const progress = $(".progress");
  const onScroll = () => {
    const y = scrollY;
    topbar.classList.toggle("scrolled", y > 12);
    const max = document.documentElement.scrollHeight - innerHeight;
    progress.style.setProperty("--p", max > 0 ? (y / max).toFixed(4) : 0);
  };
  addEventListener("scroll", onScroll, { passive: true });
  onScroll();

  // ---------------------------------------------------------- rotating words
  const words = $$(".words b");
  if (words.length && !reduce) {
    let i = 0;
    setInterval(() => {
      const cur = words[i];
      i = (i + 1) % words.length;
      cur.classList.remove("on");
      cur.classList.add("out");
      words[i].classList.remove("out");
      words[i].classList.add("on");
      setTimeout(() => cur.classList.remove("out"), 600);
    }, 2200);
  }

  // ---------------------------------------------------------- typed terminal
  // `c`: a command (typed), `o`: output (printed), `p`: a pause.
  const session = [
    { c: "runway plan --stage prod" },
    { o: '<span class="n">~ update</span> hello-prod' },
    { o: '    <span class="n">~</span> memory: 512Mi <span class="c">-></span> 1Gi' },
    { o: '    <span class="s">+</span> secrets.API_KEY: api-key@3' },
    { o: '  <span class="s">+</span> grant secretAccessor on api-key to hello-run' },
    { o: '<span class="c">1 to apply · 14 in sync</span>' },
    { p: 900 },
    { c: "runway deploy --stage prod --preview feature/login" },
    { o: '<span class="u">==></span> Building hello from . <span class="c">(image exists, skipped)</span>' },
    { o: '<span class="u">==></span> Updating Cloud Run service hello-prod' },
    { o: '<span class="ok">✓ hello-prod healthy in 38s</span>' },
    { o: 'Preview  <span class="u">https://feature-login---hello-prod-…run.app</span>' },
    { o: 'Traffic  100% hello-prod-00012 · 0% feature-login' },
    { p: 900 },
    { c: "runway deploy --stage prod --traffic 10" },
    { o: '<span class="ok">✓</span> canary: 10% hello-prod-00013 · 90% hello-prod-00012' },
    { p: 700 },
    { c: "runway traffic --stage prod --promote" },
    { o: '<span class="ok">✓</span> 100% hello-prod-00013' },
    { p: 2600 },
  ];
  const typed = $("#typed");
  if (typed) {
    const prompt = '<span class="c">$</span> ';
    if (reduce) {
      typed.innerHTML = session
        .filter((s) => !s.p)
        .map((s) => (s.c ? prompt + s.c : s.o))
        .join("\n");
    } else {
      let onScreen = false;
      visible(typed, (v) => (onScreen = v), { threshold: 0.1 });
      const waitVisible = async () => {
        while (!onScreen || document.hidden) await sleep(250);
      };
      const cursor = '<span class="cur"></span>';
      const run = async () => {
        for (;;) {
          typed.innerHTML = "";
          for (const step of session) {
            await waitVisible();
            if (step.p) {
              await sleep(step.p);
              continue;
            }
            const line = document.createElement("span");
            line.className = "line";
            typed.appendChild(line);
            if (step.c) {
              for (let k = 1; k <= step.c.length; k++) {
                line.innerHTML = prompt + step.c.slice(0, k) + cursor;
                await sleep(28 + Math.random() * 45);
              }
              await sleep(380);
              line.innerHTML = prompt + step.c;
            } else {
              line.innerHTML = step.o;
              await sleep(170);
            }
          }
          const last = document.createElement("span");
          last.className = "line";
          last.innerHTML = prompt + cursor;
          typed.appendChild(last);
          await sleep(2200);
        }
      };
      run();
    }
  }

  // ---------------------------------------------------------- counters
  $$("[data-count]").forEach((el) => {
    const end = Number(el.dataset.count);
    const suffix = el.dataset.suffix || "";
    if (reduce) {
      el.textContent = end + suffix;
      return;
    }
    const io = visible(el, (v) => {
      if (!v) return;
      io.disconnect();
      const t0 = performance.now();
      const dur = 1300;
      const tick = (t) => {
        const k = Math.min(1, (t - t0) / dur);
        const eased = 1 - Math.pow(1 - k, 3);
        el.textContent = Math.round(end * eased) + suffix;
        if (k < 1) requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    });
  });

  // ---------------------------------------------------------- spotlight cards
  $$(".spot").forEach((el) => {
    el.addEventListener("pointermove", (e) => {
      const r = el.getBoundingClientRect();
      el.style.setProperty("--mx", `${e.clientX - r.left}px`);
      el.style.setProperty("--my", `${e.clientY - r.top}px`);
    });
  });

  // ---------------------------------------------------------- pipeline
  const steps = $$(".pipe-steps li");
  const yaml = $(".yaml");
  if (steps.length && yaml) {
    const list = $(".pipe-steps");
    const activate = (n) => {
      steps.forEach((s, i) => {
        s.classList.toggle("on", i + 1 === n);
        s.classList.toggle("done", i + 1 < n);
      });
      yaml.classList.toggle("has-active", n > 0);
      $$(".ln", yaml).forEach((l) => l.classList.toggle("hl", Number(l.dataset.s) === n));
      list.style.setProperty("--fill", steps.length > 1 ? (n - 1) / (steps.length - 1) : 1);
    };
    const io = new IntersectionObserver(
      (es) => es.forEach((e) => e.isIntersecting && activate(Number(e.target.dataset.step))),
      { rootMargin: "-45% 0px -45% 0px" },
    );
    steps.forEach((s) => io.observe(s));
  }

  // ---------------------------------------------------------- traffic playground
  const traffic = $(".traffic");
  if (traffic) {
    const states = {
      preview: {
        cmd: "runway deploy --stage prod --preview feature/login",
        old: 100, neu: 0, tag: "feature-login",
        text: 'A revision with its own URL and no traffic: share <code>https://feature-login---hello-prod-…run.app</code> for review.',
      },
      canary: {
        cmd: "runway deploy --stage prod --traffic 10",
        old: 90, neu: 10, tag: "canary",
        text: "Ten percent of requests reach the new revision. Watch the logs and the metrics: nothing else moves.",
      },
      promote: {
        cmd: "runway traffic --stage prod --promote",
        old: 0, neu: 100, tag: "latest",
        text: "All traffic on the canary. The previous revision stays, ready, with its URL: no rebuild to go back.",
      },
      rollback: {
        cmd: "runway traffic --stage prod --set hello-prod-00012=100",
        old: 100, neu: 0, tag: "kept",
        text: "Back in seconds: traffic returns to the previous revision. Nothing is built or deleted.",
      },
    };
    const tabs = $$(".traffic-tabs button", traffic);
    const old = $('[data-rev="old"]', traffic);
    const neu = $('[data-rev="new"]', traffic);
    const order = Object.keys(states);
    let current = 0;
    let timer = null;
    const cycle = 4500;
    traffic.style.setProperty("--cycle", `${cycle}ms`);
    const show = (name) => {
      const s = states[name];
      current = order.indexOf(name);
      tabs.forEach((t) => t.setAttribute("aria-selected", String(t.dataset.state === name)));
      $("#traffic-cmd").textContent = s.cmd;
      $("#traffic-explain").innerHTML = s.text;
      $(".meter i", old).style.setProperty("--w", `${s.old}%`);
      $(".meter i", neu).style.setProperty("--w", `${s.neu}%`);
      $(".pct", old).textContent = `${s.old}%`;
      $(".pct", neu).textContent = `${s.neu}%`;
      $(".tag", neu).textContent = s.tag;
      // Restart the tab's progress line.
      tabs.forEach((t) => {
        t.style.animation = "none";
        void t.offsetWidth;
        t.style.animation = "";
      });
    };
    const auto = () => {
      clearInterval(timer);
      if (reduce || traffic.classList.contains("manual")) return;
      timer = setInterval(() => show(order[(current + 1) % order.length]), cycle);
    };
    tabs.forEach((t) =>
      t.addEventListener("click", () => {
        traffic.classList.add("manual");
        clearInterval(timer);
        show(t.dataset.state);
      }),
    );
    show("preview");
    visible(traffic, (v) => (v ? auto() : clearInterval(timer)), { threshold: 0.3 });
  }

  // ---------------------------------------------------------- promotion capsule
  // Hops along the rail from dev to uat to prod, resting on each stage. The
  // SVG stretches with the page, so points are mapped through its matrix.
  const rail = $("#railpath");
  const capsule = $(".capsule");
  if (rail && capsule && !reduce) {
    const flow = $(".flow");
    const envs = $$(".env", flow);
    const total = rail.getTotalLength();
    // [start, end] of each hop in the 0..1 loop; it rests in between.
    const hops = [[0.08, 0.4], [0.52, 0.84]];
    const period = 7000;
    let running = false;
    let raf = 0;
    let last = -1;
    const ease = (k) => (k < 0.5 ? 4 * k * k * k : 1 - Math.pow(-2 * k + 2, 3) / 2);
    const place = (t) => {
      let d = 0;
      let stage = 0;
      if (t >= hops[1][1]) { d = 1; stage = 2; }
      else if (t >= hops[1][0]) d = 0.5 + 0.5 * ease((t - hops[1][0]) / (hops[1][1] - hops[1][0]));
      else if (t >= hops[0][1]) { d = 0.5; stage = 1; }
      else if (t >= hops[0][0]) d = 0.5 * ease((t - hops[0][0]) / (hops[0][1] - hops[0][0]));
      const pt = rail.getPointAtLength(d * total);
      const m = rail.getCTM();
      const box = rail.ownerSVGElement.getBoundingClientRect();
      const fbox = flow.getBoundingClientRect();
      const x = pt.x * m.a + m.e + box.left - fbox.left;
      const y = pt.y * m.d + m.f + box.top - fbox.top;
      capsule.style.transform = `translate(${x}px, ${y}px)`;
      capsule.style.opacity = t > 0.96 ? String((1 - t) / 0.04) : t < 0.04 ? String(t / 0.04) : "1";
      if (stage !== last) {
        last = stage;
        envs.forEach((e, i) => e.classList.toggle("arrived", i === stage));
      }
    };
    const frame = (now) => {
      place((now % period) / period);
      if (running) raf = requestAnimationFrame(frame);
    };
    visible(flow, (v) => {
      running = v;
      cancelAnimationFrame(raf);
      if (v) raf = requestAnimationFrame(frame);
    }, { threshold: 0.1 });
  }

  // ---------------------------------------------------------- install tabs and copy
  $$(".install .tabs button").forEach((b) =>
    b.addEventListener("click", () => {
      $$(".install .tabs button").forEach((x) => x.setAttribute("aria-selected", String(x === b)));
      $("#install-cmd").textContent = b.dataset.cmd;
    }),
  );
  $$("[data-copy]").forEach((b) =>
    b.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText($(b.dataset.copy).textContent);
        b.textContent = "Copied";
        b.classList.add("done");
        setTimeout(() => {
          b.textContent = "Copy";
          b.classList.remove("done");
        }, 1600);
      } catch {
        b.textContent = "Select it";
      }
    }),
  );

  // ---------------------------------------------------------- demo
  // The exact-plan frame until the demo scrolls into view, then it plays (and loops).
  addEventListener("load", () => {
    const el = $("#demo-player");
    if (!el || !window.AsciinemaPlayer) return;
    const demo = AsciinemaPlayer.create("assets/runway-demo.cast", el, {
      fit: "width", theme: "runway", loop: true, poster: "npt:0:26", idleTimeLimit: 3,
      terminalFontFamily: getComputedStyle(document.documentElement).getPropertyValue("--mono"),
    });
    if (reduce) return;
    const io = visible(el, (v) => {
      if (v) {
        demo.play();
        io.disconnect();
      }
    }, { threshold: 0.4 });
  });
})();
