"""Generates the README "How it works" diagram, light and dark:

    python3 design/render-how-it-works.py site/assets

No packages needed. Edit the layout here, not the SVG files."""

import sys
from xml.sax.saxutils import escape

SANS = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace"

THEMES = {
    "light": dict(
        card="#ffffff", stroke="#e2e8f0", soft="#f8fafc", text="#0f172a", muted="#64748b",
        cyan="#0891b2", violet="#7c3aed", green="#059669", amber="#b45309", red="#dc2626",
        blue="#3030ef", chip="#f1f5f9", arrow="#94a3b8", shadow="0.08",
    ),
    "dark": dict(
        card="#0f1730", stroke="#26335f", soft="#0b1226", text="#e7ecff", muted="#9aa6d1",
        cyan="#22d3ee", violet="#a78bfa", green="#34d399", amber="#fbbf24", red="#f87171",
        blue="#5b5bff", chip="#16214a", arrow="#4b5a8c", shadow="0.45",
    ),
}

W, H = 1240, 612


def svg(theme):
    t = THEMES[theme]
    out = []
    add = out.append

    def text(x, y, s, size=13, weight=400, fill=None, family=SANS, anchor="start", italic=False):
        style = ' font-style="italic"' if italic else ""
        add(
            f'<text xml:space="preserve" x="{x}" y="{y}" font-family="{family}" font-size="{size}" font-weight="{weight}"'
            f' fill="{fill or t["text"]}" text-anchor="{anchor}"{style}>{escape(s)}</text>'
        )

    def card(x, y, w, h, title=None, badge=None, accent=None):
        add(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="14" fill="{t["card"]}" '
            f'stroke="{t["stroke"]}" stroke-width="1.2" filter="url(#shadow)"/>')
        if accent:
            add(f'<rect x="{x}" y="{y + 14}" width="4" height="{h - 28}" rx="2" fill="{accent}"/>')
        if title:
            text(x + 20, y + 30, title, 16, 700)
        if badge:
            bw = 9 + len(badge) * 6.6
            add(f'<rect x="{x + w - bw - 16}" y="{y + 15}" width="{bw}" height="22" rx="11" '
                f'fill="none" stroke="{t["stroke"]}"/>')
            text(x + w - bw / 2 - 16, y + 30, badge, 11, 600, t["muted"], anchor="middle")

    def chip(x, y, w, s, color, filled=False, size=12, family=SANS, h=26):
        fill = color if filled else t["chip"]
        add(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="7" fill="{fill}" '
            f'fill-opacity="{0.14 if filled else 1}" stroke="{color}" stroke-opacity="0.55"/>')
        text(x + w / 2, y + h / 2 + size * 0.36, s, size, 600, color if filled else t["text"],
             family, "middle")

    def arrow(d, dashed=False, color=None):
        dash = ' stroke-dasharray="5 5"' if dashed else ""
        head = "head-green" if color == t["green"] else "head"
        add(f'<path d="{d}" fill="none" stroke="{color or t["arrow"]}" stroke-width="1.8"'
            f' stroke-linecap="round"{dash} marker-end="url(#{head})"/>')

    add(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
        f'role="img" aria-label="How runway works">')
    add("<defs>")
    add(f'<filter id="shadow" x="-10%" y="-10%" width="120%" height="130%">'
        f'<feDropShadow dx="0" dy="4" stdDeviation="8" flood-color="#0b1020" flood-opacity="{t["shadow"]}"/></filter>')
    for name, color in (("head", t["arrow"]), ("head-green", t["green"])):
        add(f'<marker id="{name}" viewBox="0 0 12 12" refX="11" refY="6" markerWidth="12" '
            f'markerHeight="12" markerUnits="userSpaceOnUse" orient="auto">'
            f'<path d="M1.5,1.5 L11,6 L1.5,10.5 L4,6 z" fill="{color}"/></marker>')
    add(f'<linearGradient id="engine" x1="0" y1="0" x2="1" y2="1">'
        f'<stop offset="0" stop-color="{t["blue"]}"/><stop offset="1" stop-color="{t["violet"]}"/></linearGradient>')
    add("</defs>")

    # ------------------------------------------------------------ inputs --
    card(24, 36, 250, 300, "runway.yaml", accent=t["cyan"])
    yaml = [
        ("app", ": hello"), ("service", ":"), ("  source", ": ."), ("  memory", ": 512Mi"),
        ("  identity", ": { roles: … }"), ("  iap", ": { members: … }"), ("stages", ":"),
        ("  dev", ": {}"), ("  prod", ": { min_instances: 1 }"),
    ]
    for i, (k, v) in enumerate(yaml):
        y = 88 + i * 25
        add(f'<text xml:space="preserve" x="46" y="{y}" font-family="{MONO}" font-size="13">'
            f'<tspan fill="{t["violet"]}">{escape(k)}</tspan><tspan fill="{t["text"]}">{escape(v)}</tspan></text>')
    text(46, 318, "one file, next to your code", 12, 400, t["muted"], italic=True)

    card(24, 366, 250, 196, "Google Cloud", badge="live state", accent=t["green"])
    for i, s in enumerate(["service, revisions, traffic", "IAM policies, accounts",
                           "registry, buckets, secrets"]):
        add(f'<circle cx="50" cy="{415 + i * 30}" r="3.5" fill="{t["green"]}"/>')
        text(62, 420 + i * 30, s, 13)
    text(46, 540, "read before every change", 12, 400, t["muted"], italic=True)

    # ------------------------------------------------------------ engine --
    add(f'<rect x="326" y="226" width="172" height="150" rx="18" fill="url(#engine)" filter="url(#shadow)"/>')
    text(412, 276, "runway", 24, 800, "#ffffff", anchor="middle")
    text(412, 304, "desired vs live", 13, 500, "#e7ecff", anchor="middle")
    text(412, 326, "field by field", 13, 500, "#e7ecff", anchor="middle")
    text(412, 354, "no state file", 12, 700, "#ffffff", anchor="middle")
    arrow("M274,186 C302,186 290,262 306,262 L322,262")
    arrow("M274,464 C302,464 290,340 306,340 L322,340")

    # -------------------------------------------------------------- plan --
    card(548, 36, 668, 128, "runway plan", badge="read-only", accent=t["amber"])
    plan = [
        ("~ memory  512Mi → 1Gi", t["amber"]),
        ("+ IAP  group:new", t["green"]),
        ("− IAP  group:old", t["red"]),
        ("✓ exact", t["green"]),
    ]
    x = 568
    for s, color in plan:
        w = len(s) * 7.4 + 26
        chip(x, 50 + 30, w, s, color, filled=True, family=MONO, size=12)
        x += w + 12
    text(568, 142, "every change before it happens, and what is only known at deploy time",
         12.5, 400, t["muted"])
    arrow("M498,260 C524,260 512,100 528,100 L544,100")

    # ------------------------------------------------------------ deploy --
    card(548, 190, 668, 236, "runway deploy", badge="only what differs", accent=t["violet"])
    text(568, 238, "independent steps run together, in waves", 12.5, 400, t["muted"])
    waves = [
        ("1", ["APIs", "registry login"]),
        ("2", ["buckets", "secrets", "repository", "accounts"]),
        ("3", ["grants", "one lane per policy"]),
        ("4", ["build", "app grants"]),
        ("5", ["roll out", "tags · IAP", "revoke removed"]),
    ]
    cw, gap, x0, y0 = 112, 18, 568, 252
    for i, (n, chips) in enumerate(waves):
        x = x0 + i * (cw + gap)
        add(f'<circle cx="{x + 10}" cy="{y0 + 10}" r="10" fill="{t["violet"]}" fill-opacity="0.16" '
            f'stroke="{t["violet"]}" stroke-opacity="0.6"/>')
        text(x + 10, y0 + 14, n, 11, 700, t["violet"], anchor="middle")
        for j, c in enumerate(chips):
            note = c in ("one lane per policy",)
            if note:
                text(x + cw / 2, y0 + 36 + j * 32 + 17, c, 11, 400, t["muted"], anchor="middle",
                     italic=True)
            else:
                chip(x, y0 + 32 + j * 32, cw, c, t["violet"] if c in ("build", "roll out") else t["cyan"],
                     filled=c in ("build", "roll out"))
        if i < len(waves) - 1:
            arrow(f"M{x + cw + 2},{y0 + 45} L{x + cw + gap - 2},{y0 + 45}")
    add(f'<path d="M{x0 + 3 * (cw + gap) + cw / 2},{y0 + 92} l0,0" />')
    text(x0 + 3 * (cw + gap) + cw / 2, y0 + 112, "in parallel", 11, 400, t["muted"], anchor="middle",
         italic=True)
    arrow("M498,300 L544,300")

    # ----------------------------------------------------------- result --
    card(548, 452, 668, 122, "Cloud Run", badge="ready", accent=t["green"])
    urls = [
        ("hello-prod.run.app", "100%", t["green"]),
        ("feature-login---hello-prod", "0% · preview", t["cyan"]),
        ("canary---hello-prod", "10% · canary", t["violet"]),
    ]
    x = 568
    for host, share, color in urls:
        w = 26 + max(len(host) * 7.3, len(share) * 6.5)
        add(f'<rect x="{x}" y="{496}" width="{w}" height="62" rx="9" fill="{color}" fill-opacity="0.1" '
            f'stroke="{color}" stroke-opacity="0.5"/>')
        text(x + 13, 519, host, 12.5, 600, t["text"], MONO)
        text(x + 13, 543, share, 12, 600, color)
        x += w + 14
    arrow(f"M882,426 L882,448")
    arrow("M546,530 C470,530 380,516 300,516 L278,516", dashed=True, color=t["green"])
    text(426, 598, "the project is the state", 12, 400, t["muted"], anchor="middle", italic=True)

    add("</svg>")
    return "\n".join(out)


if __name__ == "__main__":
    out = sys.argv[1]
    for theme in THEMES:
        with open(f"{out}/how-it-works-{theme}.svg", "w") as f:
            f.write(svg(theme))
