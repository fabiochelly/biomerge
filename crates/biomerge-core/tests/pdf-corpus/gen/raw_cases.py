"""Hand-written raw-PDF synthetic cases: coordinates, paragraphs, fonts, low-level traps.

Usage: python3 -I raw_cases.py <corpus_dir>
Each case gets <corpus_dir>/<id>/doc.pdf, expected.txt and case.json.
expected.txt = what the page SHOWS, one paragraph per line, in visual reading order,
without page furniture (headers, footers, page numbers, watermarks).
"""
import json
import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pdfgen import Doc, StdFont, TTF, pdfstr, hexstr, wrap, std_resources, type0_font, image_xobject  # noqa: E402
from texts import PARAS  # noqa: E402

OUT = sys.argv[1]
H = StdFont("Helvetica")
HB = StdFont("Helvetica-Bold")
HI = StdFont("Helvetica-Oblique")
TR = StdFont("Times-Roman")
TI = StdFont("Times-Italic")
FONTS = {"F1": H, "F2": HB, "F3": HI, "F4": TR, "F5": TI}
FONT_OF = {v: k for k, v in FONTS.items()}

X0, TOP, WIDTH, SIZE, LEAD = 72, 770, 451, 11, 14
SPACE = H.width(" ", SIZE)  # 3.058 pt


def fresh_fonts():
    for f in FONTS.values():
        if hasattr(f, "_objnum"):
            del f._objnum


def save(cid, group, note, signal, doc, expected, checks=(), furniture=(), extra=None):
    d = os.path.join(OUT, cid)
    os.makedirs(d, exist_ok=True)
    doc.write(os.path.join(d, "doc.pdf"))
    with open(os.path.join(d, "expected.txt"), "w", encoding="utf-8") as f:
        f.write("\n".join(expected) + ("\n" if expected else ""))
    meta = {"id": cid, "group": group, "generator": "raw_cases.py (hand-written PDF syntax)",
            "note": note, "signal": signal, "checks": list(checks), "furniture": list(furniture)}
    if extra:
        meta.update(extra)
    with open(os.path.join(d, "case.json"), "w", encoding="utf-8") as f:
        json.dump(meta, f, ensure_ascii=False, indent=1)
    fresh_fonts()


def bt(res, size, x, y, text, font=None, pre=""):
    font = font or FONTS[res]
    return f"BT /{res} {size:g} Tf {pre} 1 0 0 1 {x:.3f} {y:.3f} Tm {pdfstr(font.enc(text))} Tj ET\n"


def simple_page(doc, ops, fonts=None, mediabox=(0, 0, 595, 842), extra_res="", extra=""):
    res = std_resources(doc, fonts or {"F1": H}, extra_res)
    doc.page(doc.stream(ops), res, mediabox, extra)


def layout_lines(paras, size=SIZE, lead=LEAD, width=WIDTH, top=TOP, gap=LEAD * 0.8, font=H):
    """Returns [(para_index, line_index, y, text)]."""
    out, y = [], top
    for pi, p in enumerate(paras):
        for li, line in enumerate(wrap(font, p, size, width)):
            out.append((pi, li, y, line))
            y -= lead
        y -= gap
    return out


# ---------------------------------------------------------------- coordinate & paragraph cases
def c01_shuffled_lines():
    doc = Doc()
    paras = PARAS[0:2]
    lines = layout_lines(paras)
    n = len(lines)
    mid = n // 2
    order = [mid] + [i for i in reversed(range(n)) if i != mid]
    ops = "".join(bt("F1", SIZE, X0, lines[i][2], lines[i][3]) for i in order)
    simple_page(doc, ops)
    save("c01_shuffled_lines", "coord",
         "Two paragraphs; every line is its own BT/ET with an absolute Tm; lines emitted middle first then bottom-up.",
         "Vertical position (Tm y) decides order; leading 14pt for 11pt text joins lines; an extra 11pt gap separates the paragraphs.",
         doc, paras)


def c02_words_positioned():
    doc = Doc()
    paras = PARAS[2:4]
    ops = ""
    y = TOP
    k = 0
    for p in paras:
        for line in wrap(H, p, SIZE, WIDTH):
            x = X0
            for w in line.split(" "):
                ops += bt("F1", SIZE, x, y, w)
                factor = [1.0, 0.8, 1.25, 0.9, 1.1][k % 5]
                k += 1
                x += H.width(w, SIZE) + SPACE * factor
            y -= LEAD
        y -= LEAD * 0.8
    simple_page(doc, ops)
    save("c02_words_positioned", "coord",
         "Every word is a separate text object placed with its own Tm; the PDF contains no space character at all.",
         "Horizontal gap between words = 0.8 to 1.25 x the font's space width (0.22-0.35 em): each gap must become exactly one space.",
         doc, paras, checks=[{"type": "present", "text": "Les riverains seront prévenus par courrier"}])


def c03_glyphs_kerning():
    doc = Doc()
    p1, p2 = PARAS[4], PARAS[5]
    title = "RAPPORT ANNUEL"
    ops = ""
    # title letter-spaced with Tc 3pt (real space char between the two words)
    ops += f"BT /F2 14 Tf 3 Tc 1 0 0 1 {X0} {TOP} Tm {pdfstr(title.encode('cp1252'))} Tj 0 Tc ET\n"
    y = TOP - 30
    # p1: one Tj per glyph, tiny positive gaps (0.3..0.8 pt) inside words, normal space gap between words
    k = 0
    for line in wrap(H, p1, SIZE, WIDTH - 40):
        x = X0
        for w in line.split(" "):
            for ch in w:
                ops += bt("F1", SIZE, x, y, ch)
                x += H.width(ch, SIZE) + [0.3, 0.5, 0.8, 0.4][k % 4]
                k += 1
            x += SPACE
        y -= LEAD
    y -= LEAD
    # p2: TJ arrays, kerning -40..+30 inside words, -278 (one space width) between words, no space chars
    for line in wrap(H, p2, SIZE, WIDTH):
        parts = []
        for wi, w in enumerate(line.split(" ")):
            if wi:
                parts.append("-278")
            # split the word into pairs with small kerning
            for ci, ch in enumerate(w):
                if ci:
                    parts.append(str([-40, 30, -15, 20][(ci + wi) % 4]))
                parts.append(pdfstr(ch.encode("cp1252")))
        ops += f"BT /F1 {SIZE} Tf 1 0 0 1 {X0} {y:.2f} Tm [{' '.join(parts)}] TJ ET\n"
        y -= LEAD
    simple_page(doc, ops, {"F1": H, "F2": HB})
    save("c03_glyphs_kerning", "coord",
         "Title letter-spaced with Tc=3pt; paragraph 1 drawn one glyph per Tj with 0.3-0.8pt gaps inside words; paragraph 2 drawn with TJ kerning (-40..+30/1000 em) inside words and -278/1000 em positioning instead of spaces.",
         "Inside a word the gap is <= 0.07 em (or a Tc letter-spacing): no space. Between words the gap is one space width (0.278 em): one space.",
         doc, [title, p1, p2],
         checks=[{"type": "present", "text": "RAPPORT ANNUEL"}, {"type": "absent", "text": "R A P P"}])


def c04_justified():
    doc = Doc()
    p1, p2 = PARAS[6], PARAS[7]
    ops = ""
    y = TOP
    width = 300
    lines = wrap(H, p1, SIZE, width)
    for i, line in enumerate(lines):
        nsp = line.count(" ")
        tw = 0 if i == len(lines) - 1 or nsp == 0 else (width - H.width(line, SIZE)) / nsp
        ops += f"BT /F1 {SIZE} Tf {tw:.3f} Tw 1 0 0 1 {X0} {y:.2f} Tm {pdfstr(line.encode('cp1252'))} Tj ET\n"
        y -= LEAD
    y -= LEAD
    lines = wrap(H, p2, SIZE, 250)
    widest = 0
    for i, line in enumerate(lines):
        words = line.split(" ")
        last = i == len(lines) - 1
        gap = SPACE if last or len(words) == 1 else (250 - sum(H.width(w, SIZE) for w in words)) / (len(words) - 1)
        widest = max(widest, gap)
        x = X0
        for w in words:
            ops += bt("F1", SIZE, x, y, w)
            x += H.width(w, SIZE) + gap
        y -= LEAD
    simple_page(doc, ops)
    save("c04_justified", "coord",
         f"Paragraph 1 justified with Tw word spacing on real spaces (300pt measure); paragraph 2 justified by positioning each word, no space chars (250pt measure, widest gap {widest:.1f}pt = {widest / SIZE:.2f} em).",
         "Wide but uniform inter-word gaps on a justified line are still one space; no cell/column split.",
         doc, [p1, p2])


def runs_layout(runs, width=WIDTH, x0=X0, top=TOP, lead=LEAD):
    """runs: [(font_res, size, text)]. Returns segments [(line, x, y, res, size, text)]."""
    tokens = []
    for res, size, text in runs:
        for i, w in enumerate(text.split(" ")):
            if w:
                tokens.append((res, size, w))
    segs, line, x = [], 0, x0
    cur = None
    for res, size, w in tokens:
        f = FONTS[res]
        ww = f.width(w, size)
        if x + ww > x0 + width and x > x0:
            line += 1
            x = x0
            cur = None
        y = top - line * lead
        if cur and cur[3] == res and cur[4] == size:
            cur[5] += " " + w
        else:
            cur = [line, x, y, res, size, w]
            segs.append(cur)
        x += ww + f.width(" ", size)
    return segs


def c05_font_switch():
    doc = Doc()
    runs = [("F1", SIZE, "Le premier point concerne le budget de l'année prochaine, qui reste stable."),
            ("F2", SIZE, "La seconde phrase est écrite en gras pour attirer l'attention du lecteur"),
            ("F3", SIZE, "et se prolonge en italique sur la ligne suivante,"),
            ("F4", 13, "puis une autre police plus grande prend le relais"),
            ("F1", SIZE, "avant que le texte ne reprenne normalement jusqu'à la fin du paragraphe, qui occupe plusieurs lignes.")]
    segs = runs_layout(runs)
    order = sorted(segs, key=lambda s: {"F3": 0, "F4": 1, "F2": 2, "F1": 3}[s[3]])
    ops = "".join(bt(s[3], s[4], s[1], s[2], s[5]) for s in order)
    para = " ".join(r[2] for r in runs)
    simple_page(doc, ops, FONTS)
    save("c05_font_switch", "coord",
         "One paragraph whose middle switches to bold, italic, then Times 13pt (bigger) and back; each run is a separate text object, emitted italic first, then Times, bold, regular.",
         "Same baselines and continuous x positions: one paragraph; the 13pt run on the same baseline must not start a new line or heading.",
         doc, [para])


def c06_streams_xobject():
    doc = Doc()
    para = PARAS[8] + " " + PARAS[9]
    lines = wrap(H, para, SIZE, WIDTH)
    ys = [TOP - i * LEAD for i in range(len(lines))]
    # line 1 split in two halves; second half + lines 2-3 in a Form XObject; rest in stream 2
    w1 = lines[1].split(" ")
    half = len(w1) // 2
    a, b = " ".join(w1[:half]), " ".join(w1[half:])
    s1 = bt("F1", SIZE, X0, ys[0], lines[0]) + bt("F1", SIZE, X0, ys[1], a)
    xb = bt("F1", SIZE, X0 + H.width(a + " ", SIZE), ys[1], b)
    for i in (2, 3):
        xb += bt("F1", SIZE, X0, ys[i], lines[i])
    font_res = std_resources(doc, {"F1": H})
    form = doc.stream(xb, f"/Type /XObject /Subtype /Form /BBox [0 0 595 842] /Resources {font_res}")
    s2 = "q /Fm1 Do Q\n" + "".join(bt("F1", SIZE, X0, ys[i], lines[i]) for i in range(4, len(lines)))
    c1, c2 = doc.stream(s1), doc.stream(s2)
    res = std_resources(doc, {"F1": H}, f"/XObject << /Fm1 {form} 0 R >>")
    doc.page([c1, c2], res)
    save("c06_streams_xobject", "coord",
         "One paragraph split across two content streams of the page (Contents array) and a Form XObject drawn with Do; the split falls in the middle of line 2.",
         "Visual continuity (same leading, continuous x) decides; stream and XObject boundaries must not break the paragraph.",
         doc, [para])


def c07_baseline_gaps():
    doc = Doc()
    rows = [("Nom du client", "Société Dupont"), ("Date de la commande", "12 mars 2026"),
            ("Montant total", "1 250,00 €"), ("Mode de paiement", "virement bancaire")]
    ops = bt("F2", 13, X0, TOP, "Récapitulatif de la commande")
    y = TOP - 30
    for a, b in rows:
        ops += bt("F1", SIZE, X0, y, a) + bt("F1", SIZE, 300, y, b)
        y -= LEAD * 1.3
    y -= 20
    # small gap: two text objects separated by exactly one space width
    s1, s2 = "Un petit écart entre deux objets", "reste un espace normal dans la phrase."
    ops += bt("F1", SIZE, X0, y, s1) + bt("F1", SIZE, X0 + H.width(s1, SIZE) + SPACE, y, s2)
    y -= LEAD * 2
    # tab stop: label then value after a 60pt tab on the same baseline
    ops += bt("F1", SIZE, X0, y, "Référence :") + bt("F1", SIZE, X0 + 120, y, "DOSSIER-2026-001")
    simple_page(doc, ops, {"F1": H, "F2": HB})
    exp = ["Récapitulatif de la commande"] + [f"{a} {b}" for a, b in rows] + \
          [s1 + " " + s2, "Référence : DOSSIER-2026-001"]
    checks = [{"type": "sep", "a": "client", "b": "Société", "kind": "cell"},
              {"type": "sep", "a": "commande", "b": "12", "kind": "cell"},
              {"type": "sep", "a": "total", "b": "1 250", "kind": "cell"},
              {"type": "sep", "a": "objets", "b": "reste", "kind": "space"},
              {"type": "sep", "a": "Référence :", "b": "DOSSIER", "kind": "cell"}]
    save("c07_baseline_gaps", "coord",
         "Label/value rows: label at x=72, value at x=300 on the same baseline (gap 140-200pt); a sentence made of two text objects one space width apart; a tab-stop line.",
         "Gap >> space width (here > 3 em) = cell/tab separator (newline, tab, 2+ spaces or |), never a plain space or nothing; gap = 1 space width = a single space. Each row stays on its own line (row order, not column order).",
         doc, exp, checks=checks)


def c08_leading_indent():
    doc = Doc()
    ops = ""
    y = TOP
    exp = []

    def para(text, indent=0, gap_before=0, x=X0, width=WIDTH):
        nonlocal ops, y
        y -= gap_before
        lines = wrap(H, text, SIZE, width - indent)
        first = True
        for line in lines:
            ops += bt("F1", SIZE, x + (indent if first else 0), y, line)
            if first:
                rest = " ".join(text.split(" ")[len(line.split(" ")):])
                lines2 = wrap(H, rest, SIZE, width) if rest else []
                for l2 in lines2:
                    y -= LEAD
                    ops += bt("F1", SIZE, x, y, l2)
                break
        y -= LEAD
        exp.append(text)

    para(PARAS[0])
    para(PARAS[1], gap_before=LEAD * 0.9)          # bigger vertical gap, no indent
    para(PARAS[2], indent=20)                       # first-line indent, normal leading
    para(PARAS[3], indent=20)                       # first-line indent again
    y -= LEAD * 0.9
    items = [("1.", "Premier élément de la liste, dont le texte est assez long pour continuer sur la ligne suivante avec un retrait suspendu aligné sur le texte."),
             ("2.", "Deuxième élément, court."),
             ("\u2022", "Une puce suivie d'un texte qui lui aussi déborde sur une deuxième ligne pour vérifier que la continuation reste dans le même élément.")]
    for mark, text in items:
        ops += bt("F1", SIZE, X0, y, mark)
        for li, line in enumerate(wrap(H, text, SIZE, WIDTH - 18)):
            ops += bt("F1", SIZE, X0 + 18, y, line)
            y -= LEAD
        exp.append(("" if mark == "\u2022" else mark + " ") + text)
    simple_page(doc, ops)
    save("c08_leading_indent", "coord",
         "Para 1 and 2 separated by a larger vertical gap (1.9x leading) with no indent; para 3 and 4 start with a 20pt first-line indent at normal leading; then a numbered list and a bullet list with hanging indents and continuation lines.",
         "Normal leading (14pt for 11pt) joins lines; a gap > ~1.5 x leading or a first-line indent starts a paragraph; a continuation line aligned with the item text (hanging indent) stays in the item. Bullet glyphs are dropped by the scoring normalisation.",
         doc, exp)


def c09_columns_pages():
    doc = Doc()
    colw, gutter = 210, 31
    xs = [X0, X0 + colw + gutter]
    paras = [PARAS[0], PARAS[1] + " " + PARAS[2], PARAS[3] + " " + PARAS[4] + " " + PARAS[5], PARAS[6], PARAS[7]]
    # flow lines into columns of N lines each, page header/footer
    flow = []
    for pi, p in enumerate(paras):
        for line in wrap(H, p, SIZE, colw):
            flow.append((pi, line))
        flow.append((pi, None))  # paragraph gap
    per_col = 22
    pages_ops = []
    idx = 0
    page = 0
    while idx < len(flow):
        page += 1
        ops = bt("F3", 9, X0, 805, "Rapport annuel 2026 - document de travail") + \
            bt("F3", 9, X0, 40, f"Confidentiel - page {page}")
        for col in range(2):
            y = TOP
            n = 0
            while n < per_col and idx < len(flow):
                pi, line = flow[idx]
                idx += 1
                if line is None:
                    if n == 0:
                        continue
                    y -= LEAD * 0.8
                    continue
                ops += bt("F1", SIZE, xs[col], y, line)
                y -= LEAD
                n += 1
        pages_ops.append(ops)
    res = std_resources(doc, {"F1": H, "F3": HI})
    for ops in pages_ops:
        doc.page(doc.stream(ops), res)
    save("c09_columns_pages", "coord",
         f"Two columns of 22 lines on {len(pages_ops)} pages; paragraphs continue from the bottom of column 1 to the top of column 2 and from page 1 to page 2, across a running header and a footer with a page number.",
         "Column x-band (gutter 31pt) decides column order; continuation into the next column/page has no paragraph gap; header/footer are repeated top/bottom bands (dropped).",
         doc, paras, furniture=["Rapport annuel 2026", "Confidentiel - page"])


def c10_drop_cap():
    doc = Doc()
    text = "Lorsque le soleil se leva sur la vallée, les habitants du village se rassemblèrent devant la mairie pour écouter le discours du maire, qui annonça la construction d'une nouvelle école et la rénovation complète du pont."
    rest = text[1:]
    capw = HB.width("L", 40) + 4
    ops = bt("F2", 40, X0, TOP - 2 * LEAD, "L")
    words = rest.split(" ")
    y = TOP
    lines = []
    cur = ""
    for w in words:
        width = WIDTH - (capw if len(lines) < 3 else 0)
        trial = (cur + " " + w) if cur else w
        if cur and H.width(trial, SIZE) > width:
            lines.append(cur)
            cur = w
        else:
            cur = trial
    lines.append(cur)
    for i, line in enumerate(lines):
        ops += bt("F1", SIZE, X0 + (capw if i < 3 else 0), y, line)
        y -= LEAD
    simple_page(doc, ops, {"F1": H, "F2": HB})
    save("c10_drop_cap", "coord",
         "Drop cap: a 40pt bold 'L' drawn first, its baseline on the third line; the first three lines are indented beside it and start with 'orsque'.",
         "The cap's right edge touches the first line's start and its height spans 3 lines: it belongs to the first word ('Lorsque').",
         doc, [text], checks=[{"type": "present", "text": "Lorsque le soleil"}])


def c11_superscript():
    doc = Doc()
    y = TOP
    ops = (f"BT /F1 {SIZE} Tf 1 0 0 1 {X0} {y} Tm (Selon le contrat) Tj /F1 7 Tf 4 Ts (2) Tj 0 Ts /F1 {SIZE} Tf "
           f"( signé en mars, la molécule H) Tj /F1 7 Tf -2 Ts (2) Tj 0 Ts /F1 {SIZE} Tf (O est stable et la surface vaut 12 m) Tj "
           f"/F1 7 Tf 4 Ts (2) Tj 0 Ts /F1 {SIZE} Tf (.) Tj ET\n").encode("latin-1").decode("latin-1")
    ops = ops.replace("signé", "sign\\351")
    ops += bt("F1", SIZE, X0, y - LEAD, "Le 1er janvier, la clause entre en vigueur.")
    # superscript '1er' variant via separate object: draw 'er' raised
    ops += bt("F1", 8, X0, 60, "2 Voir l'annexe B du contrat.")
    simple_page(doc, ops)
    exp = ["Selon le contrat2 signé en mars, la molécule H2O est stable et la surface vaut 12 m2. Le 1er janvier, la clause entre en vigueur.",
           "2 Voir l'annexe B du contrat."]
    save("c11_superscript", "coord",
         "Superscript footnote marker and m2 drawn with Ts +4 at 7pt, subscript 2 in H2O with Ts -2, inside one BT; a footnote at the page bottom.",
         "Baseline shift (Ts) inside a line is not a new line; the small raised/lowered glyph attaches to the preceding word (no space).",
         doc, exp,
         checks=[{"type": "any", "texts": ["contrat2 signé", "contrat 2 signé", "contrat^2 signé", "contrat²", "contrat^2^"]},
                 {"type": "any", "texts": ["H2O", "H₂O", "H~2~O"]}])


def c12_fake_bold():
    doc = Doc()
    title = "Conditions générales de vente"
    ops = bt("F1", 16, X0, TOP, title) + bt("F1", 16, X0 + 0.3, TOP, title) + bt("F1", 16, X0 + 0.3, TOP + 0.3, title)
    y = TOP - 30
    lines = wrap(H, PARAS[5], SIZE, WIDTH)
    for line in lines:
        ops += bt("F1", SIZE, X0, y, line)
        y -= LEAD
    ops += bt("F1", SIZE, X0 + 0.3, TOP - 30, lines[0])  # first line overprinted again
    simple_page(doc, ops)
    save("c12_fake_bold", "coord",
         "Fake bold: the title drawn three times with 0.3pt offsets, and the first body line drawn twice (second copy emitted after the paragraph).",
         "Same glyphs at (almost) the same position are one glyph: no duplicated text.",
         doc, [title, PARAS[5]],
         checks=[{"type": "count", "text": "Conditions générales de vente", "n": 1},
                 {"type": "count", "text": "En cas de litige", "n": 1}])


def c13_cm_tz():
    doc = Doc()
    p1, p2 = PARAS[9], PARAS[10]
    ops = "q 1.5 0 0 1.5 72 700 cm\n"
    sz = SIZE / 1.5
    for i, line in enumerate(wrap(H, p1, SIZE * 0.8, WIDTH)):
        ops += f"BT /F1 {sz:.4f} Tf 80 Tz 1 0 0 1 0 {-i * LEAD / 1.5:.3f} Tm {pdfstr(line.encode('cp1252'))} Tj ET\n"
    ops += "Q\n"
    a = math.radians(20)
    c, s = math.cos(a), math.sin(a)
    ops += f"q {c:.5f} {s:.5f} {-s:.5f} {c:.5f} 120 300 cm\n"
    for i, line in enumerate(wrap(H, p2, SIZE, 380)):
        ops += bt("F1", SIZE, 0, -i * LEAD, line)
    ops += "Q\n"
    simple_page(doc, ops)
    save("c13_cm_tz", "coord",
         "Paragraph 1 drawn inside a 1.5x scaling CTM (cm) with 7.33pt font and Tz 80 horizontal scaling; paragraph 2 drawn inside a CTM rotated 20 degrees.",
         "Positions must be computed in device space (CTM x text matrix, Tz): lines of each paragraph keep their leading; the rotated paragraph is still one paragraph.",
         doc, [p1, p2])


def c14_tagged_order():
    doc = Doc()
    pa, pb = PARAS[11], PARAS[8]
    ops = ""
    y = TOP
    for mcid, p in ((0, pa), (1, pb)):
        ops += f"/P << /MCID {mcid} >> BDC\n"
        for line in wrap(H, p, SIZE, WIDTH):
            ops += bt("F1", SIZE, X0, y, line)
            y -= LEAD
        ops += "EMC\n"
        y -= LEAD
    simple_page(doc, ops, extra="/StructParents 0")
    page_ref = lambda d: d.page_ids[0]  # noqa: E731

    def root_extra(d):
        st = d.reserve()
        docel = d.reserve()
        pb_el = d.add(f"<< /Type /StructElem /S /P /P {docel} 0 R /Pg {page_ref(d)} 0 R /K 1 >>")
        pa_el = d.add(f"<< /Type /StructElem /S /P /P {docel} 0 R /Pg {page_ref(d)} 0 R /K 0 >>")
        d.set(docel, f"<< /Type /StructElem /S /Document /P {st} 0 R /K [{pb_el} 0 R {pa_el} 0 R] >>")
        ptree = d.add(f"<< /Nums [0 [{pa_el} 0 R {pb_el} 0 R]] >>")
        d.set(st, f"<< /Type /StructTreeRoot /K {docel} 0 R /ParentTree {ptree} 0 R >>")
        return f"/StructTreeRoot {st} 0 R /MarkInfo << /Marked true >> /Lang (fr-FR)"
    doc.extra_root = root_extra
    save("c14_tagged_order", "coord",
         "Tagged PDF: paragraph A drawn at the top (MCID 0), B below (MCID 1); the structure tree lists B before A.",
         "Visual order (A then B) is the expected result; a tool that follows the structure tree outputs B first (noted, scored as an order error).",
         doc, [pa, pb])


def c15_reverse_blocks():
    doc = Doc()
    paras = PARAS[0:4]
    lines = layout_lines(paras)
    ops = ""
    for pi in reversed(range(len(paras))):
        ops += "BT /F1 11 Tf\n"
        for (p, li, y, t) in lines:
            if p == pi:
                ops += f"1 0 0 1 {X0} {y:.2f} Tm {pdfstr(t.encode('cp1252'))} Tj\n"
        ops += "ET\n"
    simple_page(doc, ops)
    save("c15_reverse_blocks", "coord",
         "Four paragraphs drawn in reverse order (last paragraph first in the content stream), lines in order inside each.",
         "Vertical position decides the paragraph order.",
         doc, paras)


# ---------------------------------------------------------------- low-level font / object cases
def l01_no_tounicode():
    doc = Doc()
    ttf = TTF("/usr/share/fonts/liberation/LiberationSans-Regular.ttf")
    f = type0_font(doc, ttf, "LiberationSans")
    paras = PARAS[0:2]
    ops = ""
    y = TOP
    for p in paras:
        for line in wrap(ttf, p, SIZE, WIDTH):
            ops += f"BT /T1 {SIZE} Tf 1 0 0 1 {X0} {y} Tm {hexstr(ttf.enc(line))} Tj ET\n"
            y -= LEAD
        y -= LEAD
    doc.page(doc.stream(ops), f"<< /Font << /T1 {f} 0 R >> >>")
    save("l01_no_tounicode", "font",
         "Type0/Identity-H TrueType font (Liberation Sans, fully embedded) with no ToUnicode CMap: codes are glyph ids.",
         "Only the embedded font's own cmap table can map glyph ids back to Unicode; a tool that ignores it outputs garbage or nothing.",
         doc, paras)


def l02_broken_tounicode():
    doc = Doc()
    ttf = TTF("/usr/share/fonts/liberation/LiberationSans-Regular.ttf")
    inv = {}
    for cp, g in ttf.cmap.items():
        inv.setdefault(g, chr(cp))
    shifted = {}
    for g, ch in inv.items():
        if ch.isalpha() and ch.isascii():
            shifted[g] = chr(ord(ch) + 1) if ch not in "zZ" else ch
        else:
            shifted[g] = ch
    partial = {g: ch for g, ch in inv.items() if not (ch.isalpha() and ch.lower() in "aeiou")}
    f1 = type0_font(doc, ttf, "LiberationSans", shifted)
    f2 = type0_font(doc, ttf, "LiberationSansB", partial)
    ops = ""
    y = TOP
    for res, p in (("T1", PARAS[2]), ("T2", PARAS[3])):
        for line in wrap(ttf, p, SIZE, WIDTH):
            ops += f"BT /{res} {SIZE} Tf 1 0 0 1 {X0} {y} Tm {hexstr(ttf.enc(line))} Tj ET\n"
            y -= LEAD
        y -= LEAD
    doc.page(doc.stream(ops), f"<< /Font << /T1 {f1} 0 R /T2 {f2} 0 R >> >>")
    save("l02_broken_tounicode", "font",
         "Paragraph 1: ToUnicode maps every ASCII letter to the next letter (a->b): the page shows correct text but the CMap lies. Paragraph 2: ToUnicode omits all vowels.",
         "Expected = what the page shows; a tool can only get it by distrusting the CMap and using the embedded font cmap. Most will output shifted text (para 1) and either drop the vowels, emit U+FFFD, or fill them from the font (para 2).",
         doc, [PARAS[2], PARAS[3]])


def l03_type3():
    doc = Doc()
    text = "Bonjour depuis une police Type trois"
    names = {}
    procs = []
    used = sorted(set(text.encode("cp1252")))
    for code in used:
        from pdfgen import WINANSI_NAMES
        n = WINANSI_NAMES[code]
        names[code] = n
        body = "600 0 d0\n" + ("" if n == "space" else "50 0 500 700 re f\n")
        procs.append(f"/{n} {doc.stream(body)} 0 R")
    diffs = " ".join(f"{c} /{names[c]}" for c in used)
    first, last = min(used), max(used)
    widths = " ".join("600" for _ in range(first, last + 1))
    font = doc.add(f"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 600 700] /FontMatrix [0.001 0 0 0.001 0 0] "
                   f"/CharProcs << {' '.join(procs)} >> /Encoding << /Type /Encoding /Differences [{diffs}] >> "
                   f"/FirstChar {first} /LastChar {last} /Widths [{widths}] /Resources << >> >>")
    ops = f"BT /T3 14 Tf 1 0 0 1 {X0} {TOP} Tm {pdfstr(text.encode('cp1252'))} Tj ET\n"
    for i, line in enumerate(wrap(H, PARAS[4], SIZE, WIDTH)):
        ops += bt("F1", SIZE, X0, TOP - 30 - i * LEAD, line)
    res = std_resources(doc, {"F1": H}, "")
    res = res[:-2] + f" >>"
    res = res.replace("/Font <<", f"/Font << /T3 {font} 0 R", 1)
    doc.page(doc.stream(ops), res)
    save("l03_type3", "font",
         "Type3 font whose glyphs are drawn boxes, named with standard glyph names via /Differences (no ToUnicode), followed by a normal Helvetica paragraph.",
         "Text recoverable only from the glyph names; visually the line is boxes (expected text = the glyph names' letters).",
         doc, [text, PARAS[4]])


def l04_accent_overstrike():
    doc = Doc()
    # TeX-style: base letter then a spacing accent glyph moved back over it
    words = [("e", "\xb4", "t"), ]
    line = "Le caf"
    ops = f"BT /F1 {SIZE} Tf 1 0 0 1 {X0} {TOP} Tm ({line}) Tj (e) Tj ET\n"
    x = X0 + H.width(line + "e", SIZE)
    ew = H.width("e", SIZE)
    aw = H.width("\xb4", SIZE)
    ops += f"BT /F1 {SIZE} Tf 1 0 0 1 {x - ew + (ew - aw) / 2:.3f} {TOP + 1} Tm (\\264) Tj ET\n"
    ops += bt("F1", SIZE, x + SPACE, TOP, "est servi chaud, d\xe9j\xe0 sucr\xe9, au go\xfbt d'\xe9t\xe9.")
    simple_page(doc, ops)
    save("l04_accent_overstrike", "font",
         "Old TeX-style accent: 'e' followed by a spacing acute accent glyph (U+00B4) drawn back over it; the rest of the line uses precomposed WinAnsi accents.",
         "Accent glyph overlapping the base letter = combining accent (expected 'café'); tools usually emit 'cafe´' or 'caf´e'.",
         doc, ["Le café est servi chaud, déjà sucré, au goût d'été."],
         checks=[{"type": "present", "text": "café est"}])


def l05_rotated_landscape():
    doc = Doc()
    res = std_resources(doc, {"F1": H, "F2": HB})
    ops = bt("F2", 14, X0, 520, "Page en paysage")
    y = 495
    for line in wrap(H, PARAS[0], SIZE, 650):
        ops += bt("F1", SIZE, X0, y, line)
        y -= LEAD
    ops += f"BT /F1 9 Tf 0 1 -1 0 800 100 Tm (Texte vertical dans la marge) Tj ET\n"
    doc.page(doc.stream(ops), res, (0, 0, 842, 595))
    # page 2: portrait MediaBox with /Rotate 90, text drawn in unrotated coordinates
    ops2 = bt("F2", 14, X0, TOP, "Page tournée de 90 degrés")
    y = TOP - 25
    for line in wrap(H, PARAS[1], SIZE, WIDTH):
        ops2 += bt("F1", SIZE, X0, y, line)
        y -= LEAD
    doc.page(doc.stream(ops2), res, (0, 0, 595, 842), "/Rotate 90")
    save("l05_rotated_landscape", "layout",
         "Page 1: landscape MediaBox with a paragraph and a 90-degree rotated line in the right margin. Page 2: portrait page with /Rotate 90.",
         "Rotated text (Tm with 90 degrees) is a separate line; /Rotate does not change the text.",
         doc, ["Page en paysage", PARAS[0], "Texte vertical dans la marge", "Page tournée de 90 degrés", PARAS[1]],
         checks=[{"type": "present", "text": "Texte vertical dans la marge"}])


def l06_forms_annots():
    doc = Doc()
    res = std_resources(doc, {"F1": H, "F2": HB})
    ops = bt("F2", 14, X0, TOP, "Formulaire d'inscription") + bt("F1", SIZE, X0, TOP - 40, "Nom :")
    ops += bt("F1", SIZE, X0, TOP - 80, PARAS[6][:90] + ".")
    content = doc.stream(ops)
    fnt = H._objnum
    ap_field = doc.stream(f"/Tx BMC BT /Helv {SIZE} Tf 2 4 Td (Jean Martin) Tj ET EMC",
                          f"/Type /XObject /Subtype /Form /BBox [0 0 200 18] /Resources << /Font << /Helv {fnt} 0 R >> >>")
    ap_free = doc.stream(f"BT /Helv 12 Tf 2 5 Td (Approuv\\351) Tj ET",
                         f"/Type /XObject /Subtype /Form /BBox [0 0 120 22] /Resources << /Font << /Helv {fnt} 0 R >> >>")
    field = doc.add(f"<< /Type /Annot /Subtype /Widget /FT /Tx /T (nom) /V (Jean Martin) /DA (/Helv {SIZE} Tf 0 g) "
                    f"/Rect [130 {TOP - 45} 330 {TOP - 27}] /F 4 /AP << /N {ap_field} 0 R >> >>")
    free = doc.add(f"<< /Type /Annot /Subtype /FreeText /Contents (Approuv\\351) /DA (/Helv 12 Tf 0 g) "
                   f"/Rect [400 {TOP - 140} 520 {TOP - 118}] /F 4 /AP << /N {ap_free} 0 R >> >>")
    note = doc.add(f"<< /Type /Annot /Subtype /Text /Contents (Note cach\\351e du relecteur) /Rect [540 {TOP} 560 {TOP + 20}] /F 4 >>")
    doc.page(content, res, extra=f"/Annots [{field} 0 R {free} 0 R {note} 0 R]")
    doc.extra_root = f"/AcroForm << /Fields [{field} 0 R] /DA (/Helv 0 Tf 0 g) /DR << /Font << /Helv {fnt} 0 R >> >> >>"
    save("l06_forms_annots", "layout",
         "AcroForm text field (value 'Jean Martin' shown by its appearance stream) beside the label 'Nom :'; a FreeText annotation 'Approuvé'; a sticky-note (Text) annotation whose contents are not displayed.",
         "Visible = page content + widget/FreeText appearances; the sticky note's contents are hidden (popup).",
         doc, ["Formulaire d'inscription", "Nom : Jean Martin", PARAS[6][:90] + ".", "Approuvé"],
         checks=[{"type": "present", "text": "Jean Martin"}, {"type": "present", "text": "Approuvé"},
                 {"type": "absent", "text": "Note cachée"}])


def l07_watermark():
    doc = Doc()
    ops = "q 0.85 g BT /F2 80 Tf 0.7071 0.7071 -0.7071 0.7071 150 250 Tm (CONFIDENTIEL) Tj ET Q\n"
    lines = layout_lines(PARAS[0:3])
    ops += "".join(bt("F1", SIZE, X0, y, t) for (_, _, y, t) in lines)
    simple_page(doc, ops, {"F1": H, "F2": HB})
    save("l07_watermark", "layout",
         "Diagonal 80pt light-grey 'CONFIDENTIEL' watermark drawn first (under the text), then three paragraphs.",
         "The watermark is furniture: big, rotated 45 degrees, grey; it must not be interleaved with the body.",
         doc, PARAS[0:3], furniture=["CONFIDENTIEL"])


def l08_empty():
    doc = Doc()
    simple_page(doc, "")
    save("l08_empty_page", "robust", "A single page with an empty content stream.", "No text, no image: the result must be empty (and ideally reported as empty, not as an error).", doc, [])
    doc = Doc()
    lines = layout_lines(PARAS[0:1])
    simple_page(doc, "".join(bt("F1", SIZE, X0, y, t) for (_, _, y, t) in lines))
    simple_page(doc, "")
    simple_page(doc, bt("F1", SIZE, X0, TOP, PARAS[1][:80] + "."))
    save("l09_blank_middle_page", "robust", "Three pages; the middle one is blank.", "A blank page adds nothing.", doc, [PARAS[0], PARAS[1][:80] + "."])


def l10_wide_line():
    doc = Doc()
    text = " ".join(PARAS)
    w = H.width(text, SIZE) + 150
    simple_page(doc, bt("F1", SIZE, 72, 100, text), mediabox=(0, 0, round(w), 200))
    save("l10_wide_line", "layout", f"One line of {len(text)} characters on a {round(w)}pt-wide page.", "One single line.", doc, [text])


def l11_ocr_layer_and_scan(tmpdir):
    """Renders a text page with gs, then builds (a) an image-only scan and (b) image + invisible text (Tr 3)."""
    import subprocess
    src = Doc()
    lines = layout_lines(PARAS[0:3])
    simple_page(src, "".join(bt("F1", SIZE, X0, y, t) for (_, _, y, t) in lines))
    os.makedirs(tmpdir, exist_ok=True)
    srcpdf = os.path.join(tmpdir, "ocr_src.pdf")
    src.write(srcpdf)
    fresh_fonts()
    ppm = os.path.join(tmpdir, "ocr_src.pgm")
    subprocess.run(["nice", "-n", "15", "gs", "-q", "-dSAFER", "-dBATCH", "-dNOPAUSE", "-sDEVICE=pgmraw", "-r100",
                    f"-sOutputFile={ppm}", srcpdf], check=True)
    for cid, invisible in (("l11_scan_image_only", False), ("l12_ocr_invisible_layer", True)):
        doc = Doc()
        img, w, h = image_xobject(doc, ppm)
        ops = "q 595 0 0 842 0 0 cm /Im1 Do Q\n"
        if invisible:
            ops += "BT 3 Tr\n" + "".join(f"/F1 {SIZE} Tf 1 0 0 1 {X0} {y:.2f} Tm {pdfstr(t.encode('cp1252'))} Tj\n" for (_, _, y, t) in lines) + "ET\n"
        res = std_resources(doc, {"F1": H}, f"/XObject << /Im1 {img} 0 R >>")
        doc.page(doc.stream(ops), res)
        if invisible:
            save(cid, "scan", "Page image (100 dpi grey raster of a text page) with an invisible text layer (render mode 3) at the same positions, like an OCRed scan.",
                 "Invisible text is the only text: an extractor must return it (it is what the page shows).", doc, PARAS[0:3])
        else:
            save(cid, "scan", "Scanned page: one 100 dpi grey image, no text operator at all.",
                 "Expected: empty text, and ideally a 'scanned/needs OCR' signal.", doc, [], extra={"expect_scanned": True})


def l13_out_of_order_words():
    """Words of one line emitted right to left (each word its own object)."""
    doc = Doc()
    ops = ""
    y = TOP
    for line in wrap(H, PARAS[9], SIZE, WIDTH):
        x = X0
        objs = []
        for w in line.split(" "):
            objs.append(bt("F1", SIZE, x, y, w))
            x += H.width(w + " ", SIZE)
        ops += "".join(reversed(objs))
        y -= LEAD
    simple_page(doc, ops)
    save("l13_words_reversed", "coord", "Each word its own text object, words of every line emitted right to left.",
         "x position decides word order within a line.", doc, [PARAS[9]])


def raw_pairs(pairs_dir):
    """p6: same visual text, different drawing code. p7: shuffled code on both sides, one word changed."""
    paras = PARAS[0:6]

    def plain(path, paras):
        doc = Doc()
        lines = layout_lines(paras)
        simple_page(doc, "".join(bt("F1", SIZE, X0, y, t) for (_, _, y, t) in lines))
        doc.write(path)
        fresh_fonts()

    def scrambled(path, paras, seed):
        import random
        doc = Doc()
        r = random.Random(seed)
        objs = []
        for (_, _, y, t) in layout_lines(paras):
            x = X0
            for w in t.split(" "):
                objs.append(bt("F1", SIZE, x, y, w))
                x += H.width(w + " ", SIZE)
        r.shuffle(objs)
        simple_page(doc, "".join(objs))
        doc.write(path)
        fresh_fonts()

    def out(pid, note, lp, rp, lexp, rexp, diffs):
        d = os.path.join(pairs_dir, pid)
        os.makedirs(d, exist_ok=True)
        lp(os.path.join(d, "left.pdf"))
        rp(os.path.join(d, "right.pdf"))
        for side, e in (("left", lexp), ("right", rexp)):
            with open(os.path.join(d, side + "_expected.txt"), "w", encoding="utf-8") as f:
                f.write("\n".join(e) + "\n")
        with open(os.path.join(d, "pair.json"), "w", encoding="utf-8") as f:
            json.dump({"id": pid, "note": note, "expected_differences": diffs}, f, ensure_ascii=False, indent=1)

    out("p6_same_visual_other_code", "Identical page; left draws one Tj per line in order, right draws every word as its own text object in random order (no space characters). No real difference.",
        lambda f: plain(f, paras), lambda f: scrambled(f, paras, 6), paras, paras, [])
    changed = list(paras)
    changed[3] = changed[3].replace("deux mois", "trois mois")
    out("p7_scrambled_word_changed", "Both sides drawn word by word in random order (different shuffles); one word changed in paragraph 4.",
        lambda f: scrambled(f, paras, 7), lambda f: scrambled(f, changed, 8), paras, changed,
        [{"left": paras[3], "right": changed[3]}])
    print("ok raw pairs")


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    for fn in [c01_shuffled_lines, c02_words_positioned, c03_glyphs_kerning, c04_justified, c05_font_switch,
               c06_streams_xobject, c07_baseline_gaps, c08_leading_indent, c09_columns_pages, c10_drop_cap,
               c11_superscript, c12_fake_bold, c13_cm_tz, c14_tagged_order, c15_reverse_blocks,
               l01_no_tounicode, l02_broken_tounicode, l03_type3, l04_accent_overstrike, l05_rotated_landscape,
               l06_forms_annots, l07_watermark, l08_empty, l10_wide_line, l13_out_of_order_words]:
        fn()
        print("ok", fn.__name__)
    l11_ocr_layer_and_scan(os.path.join(os.path.dirname(OUT.rstrip('/')), "tmp-gen"))
    print("ok l11")
    if len(sys.argv) > 2:
        raw_pairs(sys.argv[2])
