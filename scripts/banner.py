#!/usr/bin/env python3
"""Draw the README banner: the pitch on the left, a 78x19 rrover screen on the
right, laid out on a character grid in the bundled dark palette.

    python3 scripts/banner.py assets/banner.svg
"""
import sys
from html import escape
B = dict(b00="#282936", b01="#3a3c4e", b02="#4d4f68", b03="#626483", b04="#62d6e8",
         b05="#e9e9f4", b07="#f7f7fb", b0a="#00f769", b0b="#ebff87", b0d="#62d6e8",
         str="#00f769", hash="#62d6e8", list="#ffb86c", zset="#ea51b2", set="#ebff87",
         json="#b45bcf", stream="#8be9fd", local="#00f769")
W, H = 1280, 420
WX, WY, WW, WH = 598, 30, 646, 360      # terminal window
CW, RH, FS = 8.0, 16.4, 13              # cell width, row height, font size
OX, OY = WX + 12, WY + 34               # grid origin (top-left of cell 0,0)
COLS = 78
out = []
def a(s): out.append(s)
def x(c): return OX + c * CW
def top(r): return OY + r * RH
def base(r): return top(r) + RH * 0.74
def mid(r): return top(r) + RH * 0.5
def text(c, r, s, fill, bold=False, anchor=None):
    if anchor is None:            # SVG collapses leading spaces: move the start instead
        n = len(s) - len(s.lstrip(' ')); c += n; s = s.strip(' ')
    else:
        s = s.rstrip(' ')
    w = ' font-weight="700"' if bold else ''
    an = f' text-anchor="{anchor}"' if anchor else ''
    a(f'<text x="{x(c):.1f}" y="{base(r):.1f}" fill="{fill}"{w}{an}>{escape(s)}</text>')
def rtext(c_end, r, s, fill, bold=False):   # right-aligned, last char in column c_end
    text(c_end + 1, r, s, fill, bold, "end")
def hline(c0, c1, r, color):
    a(f'<line x1="{x(c0):.1f}" y1="{mid(r):.1f}" x2="{x(c1):.1f}" y2="{mid(r):.1f}" stroke="{color}"/>')
def box(c0, r0, c1, r1, color, title=None, tcolor=None, tbold=False, right=None, bottom=None):
    """Rounded border through the centres of cells (c0,r0)..(c1,r1)."""
    x0, x1 = x(c0) + CW / 2, x(c1) + CW / 2
    y0, y1 = mid(r0), mid(r1)
    a(f'<rect x="{x0:.1f}" y="{y0:.1f}" width="{x1-x0:.1f}" height="{y1-y0:.1f}" rx="5" fill="none" stroke="{color}" stroke-width="1.2"/>')
    def label(c, r, parts):
        n = sum(len(s) for s, *_ in parts)
        a(f'<rect x="{x(c)-1:.1f}" y="{top(r)+2:.1f}" width="{n*CW+2:.1f}" height="{RH-4:.1f}" fill="{B["b00"]}"/>')
        for s, fill, *bold in parts:
            text(c, r, s, fill, bool(bold and bold[0])); c += len(s)
    if title: label(c0 + 1, r0, title)
    if right:
        n = sum(len(s) for s, *_ in right); label(c1 - n, r0, right)
    if bottom: label(c0 + 1, r1, bottom)

a(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="redis-rover: a terminal UI for Redis">')
a('''<defs>
<linearGradient id="bg" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#1b1c25"/><stop offset="1" stop-color="#20212c"/></linearGradient>
<radialGradient id="glow" cx="0.2" cy="0.45" r="0.55"><stop offset="0" stop-color="#62d6e8" stop-opacity="0.16"/><stop offset="1" stop-color="#62d6e8" stop-opacity="0"/></radialGradient>
<linearGradient id="title" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#62d6e8"/><stop offset="1" stop-color="#b45bcf"/></linearGradient>
<pattern id="grid" width="28" height="28" patternUnits="userSpaceOnUse"><path d="M28 0H0V28" fill="none" stroke="#ffffff" stroke-opacity="0.03"/></pattern>
<filter id="shadow" x="-10%" y="-10%" width="120%" height="130%"><feDropShadow dx="0" dy="12" stdDeviation="16" flood-color="#000" flood-opacity="0.55"/></filter>
</defs>''')
a(f'<rect width="{W}" height="{H}" rx="18" fill="url(#bg)"/><rect width="{W}" height="{H}" rx="18" fill="url(#grid)"/><rect width="{W}" height="{H}" rx="18" fill="url(#glow)"/>')
a('<g font-family="\'JetBrains Mono\',\'SF Mono\',Menlo,Consolas,\'DejaVu Sans Mono\',monospace">')

# left: the pitch
a(f'<text x="72" y="150" font-size="22" fill="{B["b03"]}" letter-spacing="1">$ rrover</text>')
a('<text x="68" y="222" font-size="74" font-weight="700" fill="url(#title)" letter-spacing="-2">redis-rover</text>')
a(f'<text x="72" y="272" font-size="24" fill="{B["b05"]}">A terminal UI for Redis</text>')
a(f'<text x="72" y="306" font-size="18" fill="{B["b03"]}">browse · inspect · edit · monitor</text>')
a('<g font-size="13" font-weight="600">')
cx = 72
for label, color in [("write preview", B["b0a"]), ("live keys", B["b0b"]), ("Pub/Sub", B["zset"]), ("MONITOR", B["b04"])]:
    w = len(label) * 8 + 26
    a(f'<rect x="{cx}" y="334" width="{w}" height="28" rx="14" fill="{color}" fill-opacity="0.1" stroke="{color}" stroke-opacity="0.55"/>')
    a(f'<text x="{cx + w/2}" y="353" text-anchor="middle" fill="{color}">{escape(label)}</text>')
    cx += w + 10
a('</g>')

# right: the window
a(f'<g filter="url(#shadow)"><rect x="{WX}" y="{WY}" width="{WW}" height="{WH}" rx="12" fill="{B["b00"]}" stroke="#3a3c4e"/></g>')
a(f'<path d="M{WX} {WY+12}a12 12 0 0 1 12-12h{WW-24}a12 12 0 0 1 12 12v14H{WX}z" fill="#1f202a"/>')
for i, c in enumerate(["#ff5f57", "#febc2e", "#28c840"]):
    a(f'<circle cx="{WX+20+i*20}" cy="{WY+13}" r="6" fill="{c}"/>')
a(f'<text x="{WX+WW/2}" y="{WY+18}" font-size="12" text-anchor="middle" fill="{B["b03"]}">rrover — redis://localhost:6379</text>')

a(f'<g font-size="{FS}">')
# row 0: the environment line
a(f'<line x1="{x(0):.1f}" y1="{mid(0):.1f}" x2="{x(COLS):.1f}" y2="{mid(0):.1f}" stroke="{B["local"]}" stroke-width="1.5"/>')
# key list, cols 0..37, rows 1..13
K1 = 37
box(0, 1, K1, 13, B["b0d"], title=[(" Keys ", B["b05"], True)],
    bottom=[(" Page 1 · 1–7 of 7 ", B["b05"])])
text(2, 2, "filter", B["b03"]); text(9, 2, "*", B["b03"])
hline(2, K1, 3, B["b02"])
for c, s in [(4, "Type"), (11, "Key")]: text(c, 4, s, B["b03"], True)
rtext(25, 4, "TTL", B["b03"], True); rtext(35, 4, "Size", B["b03"], True)
hline(2, K1, 5, B["b02"])
rows = [("hash", "user:1001", "∞", "312 B"), ("hash", "user:1002", "∞", "296 B"),
        ("str", "session:7…", "1h", "96 B"), ("list", "queue:mail", "∞", "2.00 KiB"),
        ("set", "tags:popular", "∞", "540 B"), ("zset", "board:top", "1d", "1.17 KiB"),
        ("json", "app:config:…", "∞", "420 B")]
for i, (t, k, ttl, size) in enumerate(rows):
    r = 6 + i
    sel = i == 0
    if sel:
        a(f'<rect x="{x(1):.1f}" y="{top(r):.1f}" width="{(K1-1)*CW:.1f}" height="{RH:.1f}" fill="{B["b02"]}"/>')
        text(2, r, "▌", B["b05"])
    dim = B["b05"] if sel else B["b03"]
    text(4, r, t, B[t], True)
    text(11, r, k, B["b05"], sel)
    rtext(25, r, ttl, dim if ttl == "∞" else B["b0b"], sel)
    rtext(35, r, size, dim, sel)

# value pane, cols 38..77
V0, V1 = 38, COLS - 1
box(V0, 1, V1, 13, B["b02"], title=[(" Values ", B["b03"])], right=[(" ● live ", B["b0b"])])
for r, (k, v) in enumerate([("Key", "user:1001"), ("Type", "hash"), ("TTL", "No expiry"), ("Size", "312 B")], start=2):
    text(V0 + 2, r, k, B["b03"]); text(V0 + 8, r, v, B["b05"])
hline(V0 + 2, V1, 6, B["b02"])
text(V0 + 4, 7, "Field", B["b03"], True); text(V0 + 18, 7, "Value", B["b03"], True)
hline(V0 + 2, V1, 8, B["b02"])
for i, (f, v) in enumerate([("name", "Ada"), ("email", "ada@example.com"), ("plan", "pro"), ("visits", "42")]):
    r = 9 + i
    changed = f == "visits"
    if changed:
        a(f'<rect x="{x(V0+1):.1f}" y="{top(r):.1f}" width="{(V1-V0-1)*CW:.1f}" height="{RH:.1f}" fill="{B["b0a"]}"/>')
    fg = B["b00"] if changed else B["b05"]
    text(V0 + 4, r, f, fg); text(V0 + 18, r, v, fg)

# hint bar
c = 1
for key, what in [("j", "down"), ("k", "up"), ("Enter", "open"), ("f", "filter"), ("o", "sort"),
                  ("v", "tree"), ("a", "add"), ("d", "del"), ("?", "help")]:
    text(c, 14, key, B["b04"]); text(c + len(key) + 1, 14, what, B["b03"]); c += len(key) + len(what) + 3

# info footer, rows 15..17
box(0, 15, COLS - 1, 17, B["b02"], title=[(" Info · redis://localhost:6379 ", B["b04"])],
    right=[(" ? Help ", B["b04"])])
bx = 1 + len(" Info · redis://localhost:6379 ") + 1
a(f'<rect x="{x(bx):.1f}" y="{top(15)+1:.1f}" width="{7*CW:.1f}" height="{RH-2:.1f}" fill="{B["local"]}"/>')
text(bx, 15, " LOCAL ", B["b00"], True)
text(2, 16, "Redis version: 8.0.2", B["b04"]); text(32, 16, "db 0 · 21 keys", B["b04"])
rtext(75, 16, "CPU: 0.4 %", B["b04"])

# tab strip
c = 1
for i, tab in enumerate(["Keys", "Dashboard", "Pub/Sub", "Monitor", "Slowlog"]):
    s = f" {tab} "
    if i == 0:
        a(f'<rect x="{x(c):.1f}" y="{top(18)+1:.1f}" width="{len(s)*CW:.1f}" height="{RH-2:.1f}" fill="{B["b05"]}"/>')
        text(c, 18, s, B["b00"], True)
    else:
        text(c, 18, s, B["b04"])
    c += len(s)
a('</g></g></svg>')
open(sys.argv[1], "w").write("\n".join(out) + "\n")
