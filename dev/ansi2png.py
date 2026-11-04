"""Renders a colour capture of a terminal (`tmux capture-pane -p -e`) to a PNG.

Usage: python3 -I dev/ansi2png.py capture.txt out.png [COLUMNS] [light]
"""
import re, sys
from PIL import Image, ImageDraw, ImageFont
src, out = sys.argv[1], sys.argv[2]
light = 'light' in sys.argv[3:]
width = next((int(a) for a in sys.argv[3:] if a.isdigit()), 0)
FG, BG = ((40, 40, 40), (250, 250, 248)) if light else ((214, 214, 214), (24, 24, 28))
PAL = [(0,0,0),(205,49,49),(13,188,121),(229,229,16),(36,114,200),(188,63,188),(17,168,205),(229,229,229),
       (102,102,102),(241,76,76),(35,209,139),(245,245,67),(59,142,234),(214,112,214),(41,184,219),(255,255,255)]
if light: PAL[7] = (80, 80, 80)
def c256(n):
    if n < 16: return PAL[n]
    if n < 232:
        n -= 16; v = [0, 95, 135, 175, 215, 255]
        return (v[n // 36], v[(n // 6) % 6], v[n % 6])
    g = 8 + (n - 232) * 10; return (g, g, g)
font = ImageFont.truetype('/System/Library/Fonts/Menlo.ttc', 26)
CW, CH = 16, 32
lines = open(src, encoding='utf-8').read().split('\n')
if lines and lines[-1] == '': lines.pop()
cols = 0
cells = []
for line in lines:
    row = []; st = {'fg': None, 'bg': None, 'bold': False, 'dim': False, 'rev': False, 'it': False}
    for tok in re.split(r'(\x1b\[[0-9;]*m)', line):
        if tok.startswith('\x1b['):
            ps = [int(p) if p else 0 for p in tok[2:-1].split(';')] or [0]
            i = 0
            while i < len(ps):
                p = ps[i]
                if p == 0: st = {'fg': None, 'bg': None, 'bold': False, 'dim': False, 'rev': False, 'it': False}
                elif p == 1: st['bold'] = True
                elif p == 2: st['dim'] = True
                elif p == 3: st['it'] = True
                elif p == 7: st['rev'] = True
                elif p == 22: st['bold'] = st['dim'] = False
                elif p == 27: st['rev'] = False
                elif 30 <= p <= 37: st['fg'] = PAL[p - 30]
                elif 90 <= p <= 97: st['fg'] = PAL[p - 90 + 8]
                elif 40 <= p <= 47: st['bg'] = PAL[p - 40]
                elif p == 39: st['fg'] = None
                elif p == 49: st['bg'] = None
                elif p in (38, 48):
                    key = 'fg' if p == 38 else 'bg'
                    if ps[i+1] == 5: st[key] = c256(ps[i+2]); i += 2
                    elif ps[i+1] == 2: st[key] = tuple(ps[i+2:i+5]); i += 4
                i += 1
        else:
            for ch in tok: row.append((ch, dict(st)))
    cells.append(row); cols = max(cols, len(row))
cols = max(cols, width)
img = Image.new('RGB', (cols * CW + 32, len(cells) * CH + 32), BG)
d = ImageDraw.Draw(img)
for y, row in enumerate(cells):
    for x, (ch, st) in enumerate(row):
        fg, bg = st['fg'] or FG, st['bg'] or BG
        if st['rev']: fg, bg = bg, fg
        if st['dim']: fg = tuple(int(a * 0.55 + b * 0.45) for a, b in zip(fg, bg))
        px, py = 16 + x * CW, 16 + y * CH
        if bg != BG: d.rectangle([px, py, px + CW, py + CH], fill=bg)
        if ch == '█': d.rectangle([px, py, px + CW - 1, py + CH - 1], fill=fg)
        elif ch == '▀': d.rectangle([px, py, px + CW - 1, py + CH // 2 - 1], fill=fg)
        elif ch == '▄': d.rectangle([px, py + CH // 2, px + CW - 1, py + CH - 1], fill=fg)
        elif ch in '─│╭╮╰╯':
            mx, my = px + CW // 2, py + CH // 2
            r = CW // 2
            if ch == '─': d.line([px, my, px + CW, my], fill=fg, width=2)
            elif ch == '│': d.line([mx, py, mx, py + CH], fill=fg, width=2)
            elif ch == '╭': d.arc([mx, my, mx + 2 * r, my + 2 * r], 180, 270, fill=fg, width=2); d.line([mx, my + r, mx, py + CH], fill=fg, width=2)
            elif ch == '╮': d.arc([mx - 2 * r, my, mx, my + 2 * r], 270, 360, fill=fg, width=2); d.line([mx, my + r, mx, py + CH], fill=fg, width=2)
            elif ch == '╰': d.arc([mx, my - 2 * r, mx + 2 * r, my], 90, 180, fill=fg, width=2); d.line([mx, py, mx, my - r], fill=fg, width=2)
            elif ch == '╯': d.arc([mx - 2 * r, my - 2 * r, mx, my], 0, 90, fill=fg, width=2); d.line([mx, py, mx, my - r], fill=fg, width=2)
        elif ch != ' ': d.text((px, py + 2), ch, font=font, fill=fg)
img.save(out)
