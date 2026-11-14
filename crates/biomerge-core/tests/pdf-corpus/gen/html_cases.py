"""HTML/CSS synthetic cases printed to PDF by headless Chromium (Skia PDF backend).

Usage: python3 -I html_cases.py <corpus_dir> <pairs_dir> <big_dir>
"""
import html
import json
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from texts import PARAS, paragraph, rng  # noqa: E402

CORPUS, PAIRS, BIG = sys.argv[1], sys.argv[2], sys.argv[3]
HERE = os.path.dirname(os.path.abspath(__file__))
PROFILE = os.path.join(HERE, "chrome-profile")

BASE_CSS = """
@page { size: A4; margin: 25mm 20mm 25mm 20mm; }
body { font-family: 'Liberation Serif', serif; font-size: 11pt; line-height: 1.35; }
p { margin: 0 0 0.6em 0; }
"""


def esc(s):
    return html.escape(s, quote=False)


def chromium_pdf(html_path, pdf_path):
    env = dict(os.environ)
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    subprocess.run(["nice", "-n", "15", "chromium", "--headless=new", "--no-sandbox", "--disable-gpu",
                    f"--user-data-dir={PROFILE}", "--no-pdf-header-footer", "--no-first-run",
                    "--disable-extensions", "--disable-background-networking",
                    f"--print-to-pdf={pdf_path}", "file://" + html_path],
                   env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=600)


def page_html(body, css="", lang="fr"):
    return (f"<!doctype html><html lang='{lang}'><head><meta charset='utf-8'><style>{BASE_CSS}{css}</style>"
            f"</head><body>{body}</body></html>")


def write_case(root, cid, group, note, signal, body, expected, css="", checks=(), furniture=(), lang="fr", extra=None):
    d = os.path.join(root, cid)
    os.makedirs(d, exist_ok=True)
    hp = os.path.join(d, "doc.html")
    with open(hp, "w", encoding="utf-8") as f:
        f.write(page_html(body, css, lang))
    chromium_pdf(hp, os.path.join(d, "doc.pdf"))
    with open(os.path.join(d, "expected.txt"), "w", encoding="utf-8") as f:
        f.write("\n".join(expected) + "\n")
    meta = {"id": cid, "group": group, "generator": "html_cases.py (Chromium print-to-pdf)", "note": note,
            "signal": signal, "checks": list(checks), "furniture": list(furniture)}
    if extra:
        meta.update(extra)
    with open(os.path.join(d, "case.json"), "w", encoding="utf-8") as f:
        json.dump(meta, f, ensure_ascii=False, indent=1)
    print("ok", cid, flush=True)


HEADER_CSS = """
@page { @top-left { content: "Société Exemple - Rapport interne"; font: italic 9pt 'Liberation Sans'; }
        @top-right { content: "Réf. RI-2026-04"; font: 9pt 'Liberation Sans'; }
        @bottom-center { content: "Page " counter(page) " / " counter(pages); font: 9pt 'Liberation Sans'; } }
"""
FURN = ["Société Exemple - Rapport interne", "Réf. RI-2026-04", "Page "]


def blocks_to_html(blocks):
    """blocks: list of (tag, text). Returns (html, expected_lines)."""
    out, exp = [], []
    for tag, text in blocks:
        out.append(f"<{tag}>{esc(text)}</{tag}>")
        exp.append(text)
    return "\n".join(out), exp


def long_doc_blocks(seed, n, with_headings=True):
    r = rng(seed)
    blocks = []
    for i in range(n):
        if with_headings and i % 5 == 0:
            blocks.append(("h2", f"Section {i // 5 + 1} : {r.choice(['Objectifs', 'Résultats', 'Méthode', 'Budget', 'Calendrier'])}"))
        blocks.append(("p", PARAS[i % len(PARAS)] if i < len(PARAS) else paragraph(r)))
    return blocks


def cases():
    # 1. paragraphs across page breaks + running header/footer
    blocks = [("h1", "Rapport interne sur l'activité")] + long_doc_blocks(1, 22)
    body, exp = blocks_to_html(blocks)
    write_case(CORPUS, "h01_paragraphs_pages", "layout",
               "Three-page report: headings, paragraphs split across page breaks, @page running header (left+right) and footer 'Page N / M'.",
               "Paragraph continues across page break; header/footer repeated on every page.",
               body, exp, HEADER_CSS, furniture=FURN)

    # 2. hyphenation: soft hyphens (manual) and real compound hyphens, narrow justified column
    words_soft = ["extra­ordinaire", "anti­constitutionnelle­ment", "dé­veloppe­ment",
                  "ad­ministra­tion", "res­ponsa­bilité", "in­compré­hen­sible"]
    compounds = ["porte-monnaie", "arc-en-ciel", "c'est-à-dire", "vis-à-vis", "après-midi", "grand-mère"]
    r = rng(2)
    paras = []
    for i in range(8):
        ws = []
        for _ in range(26):
            ws.append(r.choice(["le", "la", "un", "une", "grand", "petit", "notre", "votre", "très", "bien", "dossier", "village"]))
        for j, w in enumerate([words_soft[i % 6], compounds[i % 6], words_soft[(i + 3) % 6], compounds[(i + 2) % 6]]):
            ws.insert(4 + j * 6, w)
        paras.append(" ".join(ws).capitalize() + ".")
    body = "".join(f"<p>{esc(p)}</p>" for p in paras)
    exp = [p.replace("­", "") for p in paras]
    css = "body{width:5.2cm; text-align: justify; hyphens: manual;} "
    write_case(CORPUS, "h02_hyphenation", "text",
               "Narrow (5.2cm) justified column; long words carry soft hyphens (U+00AD, CSS hyphens: manual) so Chromium breaks them with a visible hyphen; compounds with real hyphens (porte-monnaie, c'est-à-dire...) may break after their hyphen.",
               "A hyphen at line end inside a soft-hyphenated word must disappear when joining; a real compound hyphen must stay ('porte-monnaie', not 'portemonnaie').",
               body, exp, css,
               checks=[{"type": "present", "text": c} for c in compounds] +
                      [{"type": "present", "text": w.replace("­", "")} for w in words_soft])

    # 3/4. columns
    for n, cid in ((2, "h03_two_columns"), (3, "h04_three_columns")):
        blocks = long_doc_blocks(3 + n, 10, with_headings=False)
        body = "<h1>Lettre d'information</h1><div class='cols'>" + "".join(f"<p>{esc(t)}</p>" for _, t in blocks) + "</div>"
        exp = ["Lettre d'information"] + [t for _, t in blocks]
        write_case(CORPUS, cid, "layout", f"{n} CSS columns (column-gap 8mm) of justified paragraphs under a full-width title; paragraphs continue from one column to the next.",
                   "Column x-bands; paragraph continues at the top of the next column.",
                   body, exp, f".cols{{column-count:{n}; column-gap:8mm; text-align:justify;}}")

    # 5. simple table
    rows = [("Article", "Quantité", "Prix unitaire", "Total"), ("Clavier sans fil", "2", "45,00 €", "90,00 €"),
            ("Souris optique", "3", "19,90 €", "59,70 €"), ("Écran 27 pouces", "1", "289,00 €", "289,00 €"),
            ("Câble HDMI", "4", "8,50 €", "34,00 €"), ("Total TTC", "", "", "472,70 €")]
    tcss = "table{border-collapse:collapse} td,th{border:1px solid #444; padding:3px 8px}"

    def table_html(rows, head=True):
        h = "<table>"
        for i, row in enumerate(rows):
            tag = "th" if (head and i == 0) else "td"
            h += "<tr>" + "".join(f"<{tag}>{esc(c)}</{tag}>" for c in row) + "</tr>"
        return h + "</table>"
    body = "<p>Détail de la commande ci-dessous.</p>" + table_html(rows) + "<p>Merci de votre confiance.</p>"
    exp = ["Détail de la commande ci-dessous."] + [" ".join(c for c in r if c) for r in rows] + ["Merci de votre confiance."]
    write_case(CORPUS, "h05_table_simple", "table", "Bordered 4-column table, 6 rows, empty cells in the total row.",
               "Each row on one line, cells in column order (cell separator free: |, tab or spaces).", body, exp, tcss,
               checks=[{"type": "sep", "a": "Clavier sans fil", "b": "2", "kind": "cell"}])

    # 6. merged cells
    body = """<table>
<tr><th rowspan='2'>Région</th><th colspan='2'>Ventes 2025</th></tr>
<tr><th>Premier semestre</th><th>Second semestre</th></tr>
<tr><td>Nord</td><td>1 200</td><td>1 350</td></tr>
<tr><td rowspan='2'>Sud</td><td>980</td><td>1 010</td></tr>
<tr><td colspan='2'>Données consolidées en attente</td></tr>
<tr><td>Est</td><td>760</td><td>805</td></tr></table>"""
    exp = ["Région Ventes 2025", "Premier semestre Second semestre", "Nord 1 200 1 350", "Sud 980 1 010",
           "Données consolidées en attente", "Est 760 805"]
    write_case(CORPUS, "h06_table_merged", "table", "Table with rowspan and colspan cells (two-level header, a 2-row 'Sud' cell, a 2-column note).",
               "Row-major reading; a rowspanned cell appears once, on its first row.", body, exp, tcss)

    # 7. long table spanning pages with repeated header
    r = rng(7)
    trs = []
    exp = ["Relevé des opérations"]
    for i in range(1, 121):
        d = f"{(i % 28) + 1:02d}/0{(i % 9) + 1}/2026"
        lib = r.choice(["Virement reçu", "Prélèvement électricité", "Carte supermarché", "Retrait espèces", "Abonnement téléphone"])
        amt = f"{r.randint(5, 900)},{r.randint(0, 99):02d}"
        trs.append(f"<tr><td>{d}</td><td>{esc(lib)}</td><td>{amt}</td></tr>")
        exp.append(f"{d} {lib} {amt}")
    body = ("<h2>Relevé des opérations</h2><table><thead><tr><th>Date</th><th>Libellé</th><th>Montant</th></tr></thead><tbody>"
            + "".join(trs) + "</tbody></table>")
    exp.insert(1, "Date Libellé Montant")
    write_case(CORPUS, "h07_table_long", "table", "120-row table over 3+ pages; the header row (thead) is repeated by Chromium at the top of every page.",
               "The repeated header row on pages 2+ is furniture (expected once).", body, exp, tcss,
               furniture=[], extra={"repeated_line": "Date Libellé Montant"})

    # 8. nested lists
    body = """<p>Points à traiter :</p><ul><li>Organisation de la réunion<ul><li>Réserver la salle du deuxième étage</li>
<li>Envoyer l'ordre du jour<ol><li>Rédiger le brouillon</li><li>Le faire valider par la direction</li></ol></li></ul></li>
<li>Préparation du budget, en tenant compte des dépenses imprévues du dernier trimestre et des nouvelles embauches prévues au printemps</li>
<li>Questions diverses</li></ul><ol><li>Première étape</li><li>Deuxième étape</li></ol>"""
    exp = ["Points à traiter :", "Organisation de la réunion", "Réserver la salle du deuxième étage", "Envoyer l'ordre du jour",
           "1. Rédiger le brouillon", "2. Le faire valider par la direction",
           "Préparation du budget, en tenant compte des dépenses imprévues du dernier trimestre et des nouvelles embauches prévues au printemps",
           "Questions diverses", "1. Première étape", "2. Deuxième étape"]
    write_case(CORPUS, "h08_lists_nested", "text", "Bulleted list nested 3 levels (bullet, circle, numbers), one long item wrapping over two lines, then a numbered list.",
               "Each item on one line; bullet glyphs dropped by the scoring normalisation, numbers kept.", body, exp, "body{width:12cm}")

    # 9. footnotes at the bottom of each page
    pages = []
    exp_body, exp_notes = [], []
    for pg in range(2):
        p1, p2 = PARAS[pg * 2], PARAS[pg * 2 + 1]
        n1, n2 = pg * 2 + 1, pg * 2 + 2
        a = p1.replace(".", f"<sup>{n1}</sup>.", 1)
        b = p2.replace(".", f"<sup>{n2}</sup>.", 1)
        notes = [f"{n1} Source : archives de la société, 2025.", f"{n2} Voir également le rapport annuel."]
        pages.append(f"<div class='pg'><p>{a}</p><p>{b}</p><div class='fn'>" + "".join(f"<p>{esc(x)}</p>" for x in notes) + "</div></div>")
        exp_body += [p1.replace(".", f"{n1}.", 1), p2.replace(".", f"{n2}.", 1)]
        exp_notes += notes
    css = (".pg{position:relative; height:240mm; break-after:page} .fn{position:absolute; bottom:0; font-size:8.5pt; "
           "border-top:1px solid #000; width:60mm} sup{font-size:7pt}")
    # expected: notes after the body of their own page
    exp = [exp_body[0], exp_body[1], exp_notes[0], exp_notes[1], exp_body[2], exp_body[3], exp_notes[2], exp_notes[3]]
    write_case(CORPUS, "h09_footnotes", "text", "Two pages, each with two paragraphs carrying superscript note calls and two footnotes at the page bottom.",
               "Footnotes follow their page's body; the superscript call sticks to the word before the period.", "".join(pages), exp, css)

    # 10. headings structure with inline emphasis
    body = """<h1>Guide de l'utilisateur</h1><h2>1. Installation</h2><p>Téléchargez le programme puis lancez l'installateur. <b>Ne débranchez pas</b> l'appareil pendant la mise à jour, <i>même si elle semble bloquée</i>.</p>
<h3>1.1 Configuration requise</h3><p>Un ordinateur récent avec au moins huit gigaoctets de mémoire.</p><h2>2. Utilisation</h2><p>Ouvrez le menu <b><i>Fichier</i></b> et choisissez <code>Nouveau projet</code>.</p>"""
    exp = ["Guide de l'utilisateur", "1. Installation",
           "Téléchargez le programme puis lancez l'installateur. Ne débranchez pas l'appareil pendant la mise à jour, même si elle semble bloquée.",
           "1.1 Configuration requise", "Un ordinateur récent avec au moins huit gigaoctets de mémoire.", "2. Utilisation",
           "Ouvrez le menu Fichier et choisissez Nouveau projet."]
    write_case(CORPUS, "h10_headings", "text", "h1/h2/h3 headings, bold, italic, bold-italic and monospace inline runs.",
               "Font size/weight changes inside a line do not split it; headings are separate lines.", body, exp)

    # 11. ligatures
    t = "Les officiers ont affiché une efficacité affligeante face au flux de réfugiés ; la finance et les fleurs du souffle offrent un fief difficile."
    body = f"<p>{esc(t)}</p><p style=\"font-family:'Libertinus Serif'\">{esc(t)}</p><p style=\"font-family:'Libertinus Serif'; font-variant-ligatures: discretionary-ligatures\">{esc(t)}</p>"
    write_case(CORPUS, "h11_ligatures", "text", "Same sentence in Liberation Serif, Libertinus Serif (standard fi/fl/ffi/ff ligatures) and Libertinus with discretionary ligatures.",
               "Ligature glyphs must come out as their letters (fi, ffi...), not U+FB01 or nothing.", body, [t, t, t],
               checks=[{"type": "count", "text": "officiers", "n": 3}, {"type": "count", "text": "efficacité", "n": 3},
                       {"type": "count", "text": "affligeante", "n": 3}, {"type": "count", "text": "flux", "n": 3}])

    # 12. justified
    body = "".join(f"<p>{esc(p)}</p>" for p in PARAS[0:5])
    write_case(CORPUS, "h12_justified", "text", "Five justified paragraphs in a 9cm measure (wide variable inter-word gaps).",
               "Justification gaps are single spaces.", body, PARAS[0:5], "body{width:9cm; text-align:justify}")

    # 13. accents composed vs decomposed
    import unicodedata
    t = "Été comme hiver, la forêt du château reste ouverte ; à côté, l'élève déjà âgé où naïf Noël."
    d = unicodedata.normalize("NFD", t)
    body = f"<p>{esc(t)}</p><p>{esc(d)}</p>"
    write_case(CORPUS, "h13_accents_nfd", "text", "Same sentence in precomposed (NFC) and decomposed (NFD, base + combining accent) form in the HTML source.",
               "After NFC both lines must be identical to the expected; raw output may keep combining marks.", body, [t, t],
               checks=[{"type": "count", "text": "château", "n": 2}])

    # 14. RTL / mixed
    ar = "مرحبا بكم في المكتبة"
    he = "שלום עולם"
    lines = [f"Le mot arabe {ar} signifie bienvenue à la bibliothèque.", ar + " 2026", f"En hébreu on écrit {he} pour dire bonjour."]
    body = f"<p>{esc(lines[0])}</p><p dir='rtl' style=\"font-family:'Noto Sans Arabic'\">{esc(lines[1])}</p><p>{esc(lines[2])}</p>"
    write_case(CORPUS, "h14_rtl_mixed", "script", "French sentence with an embedded Arabic phrase, an RTL Arabic paragraph with a number, and an embedded Hebrew phrase.",
               "Expected in logical (reading) order, as typed; visual-order output reverses the Arabic/Hebrew letters.", body, lines,
               checks=[{"type": "present", "text": ar}, {"type": "present", "text": he}])

    # 15. CJK
    zh = "今天天气很好，我们去公园散步吧。"
    ja = "東京は日本の首都です。桜の季節はとても美しい。"
    ko = "안녕하세요, 만나서 반갑습니다."
    body = f"<p lang='zh'>{zh}</p><p lang='ja'>{ja}</p><p lang='ko'>{ko}</p><p>Fin du texte multilingue.</p>"
    write_case(CORPUS, "h15_cjk", "script", "Chinese, Japanese and Korean paragraphs (Noto CJK / Source Han fonts, Type0 CID).",
               "No spaces inserted between CJK ideographs.", body, [zh, ja, ko, "Fin du texte multilingue."],
               checks=[{"type": "present", "text": zh}, {"type": "present", "text": ja}])

    # 16. emoji
    t = "Livraison prévue demain 🚚 merci pour votre commande 🙏 et bonne journée ☀️ !"
    write_case(CORPUS, "h16_emoji", "script", "Colour emoji (Noto Color Emoji bitmap glyphs) inside a sentence.",
               "Emoji glyphs are Type3/bitmap in Chromium output; ToUnicode should give the code points.", f"<p>{esc(t)}</p>", [t],
               checks=[{"type": "present", "text": "merci pour votre commande"}])

    # 17. small caps + uppercase transform
    body = "<p>L'auteur <span style='font-variant:small-caps'>Victor Hugo</span> est né à Besançon.</p><p>Le titre <span style='text-transform:uppercase'>les misérables</span> paraît en 1862.</p>"
    exp = ["L'auteur Victor Hugo est né à Besançon.", "Le titre LES MISÉRABLES paraît en 1862."]
    write_case(CORPUS, "h17_small_caps", "text", "Synthesised small caps (Liberation Serif has no smcp) and CSS uppercase transform.",
               "The page shows capitals; Chromium keeps the source casing in the text layer for synthesised small caps and the glyph capitals for text-transform, so expected follows that; a case-insensitive check is also recorded.",
               body, exp, checks=[{"type": "present_ci", "text": "victor hugo est né"}, {"type": "present_ci", "text": "les misérables paraît"}])

    # 18. sup / sub
    t = "Le 1er janvier, la formule H2O et E = mc2 occupent 30 m2 du tableau."
    body = "<p>Le 1<sup>er</sup> janvier, la formule H<sub>2</sub>O et E = mc<sup>2</sup> occupent 30 m<sup>2</sup> du tableau.</p>"
    write_case(CORPUS, "h18_sup_sub", "text", "Superscript and subscript runs (HTML sup/sub: smaller font, shifted baseline).",
               "Shifted run joins the preceding word without a space and stays on the line.", body, [t],
               checks=[{"type": "any", "texts": ["1er janvier", "1 er janvier", "1^er^"]}, {"type": "any", "texts": ["H2O", "H 2 O", "H~2~O"]}])

    # 19. landscape page in the middle
    body = (f"<p>{esc(PARAS[0])}</p><div class='land'><h2>Annexe en paysage</h2><p>{esc(PARAS[1])}</p></div><p>{esc(PARAS[2])}</p>")
    css = "@page wide { size: A4 landscape; } .land { page: wide; }"
    write_case(CORPUS, "h19_landscape_page", "layout", "Portrait page, then a named @page in A4 landscape, then portrait again.",
               "Page orientation changes nothing in the text.", body, [PARAS[0], "Annexe en paysage", PARAS[1], PARAS[2]], css)

    # 20. invoice-like layout (blocks left/right)
    body = """<div style='display:flex; justify-content:space-between'><div>Atelier Martin<br>12 rue des Lilas<br>75014 Paris</div>
<div style='text-align:right'>Facture n° F-2026-118<br>Date : 3 février 2026<br>Échéance : 5 mars 2026</div></div>
<p style='margin-top:12mm'>Client : Librairie du Centre, 4 place de la Gare, 69002 Lyon</p>
<table><tr><th>Désignation</th><th>Montant HT</th></tr><tr><td>Reliure de 40 ouvrages</td><td>640,00 €</td></tr>
<tr><td>Restauration de couvertures</td><td>210,00 €</td></tr></table>
<p style='text-align:right'>Total HT : 850,00 €<br>TVA 20 % : 170,00 €<br>Total TTC : 1 020,00 €</p>"""
    exp = ["Atelier Martin", "12 rue des Lilas", "75014 Paris", "Facture n° F-2026-118", "Date : 3 février 2026", "Échéance : 5 mars 2026",
           "Client : Librairie du Centre, 4 place de la Gare, 69002 Lyon", "Désignation Montant HT", "Reliure de 40 ouvrages 640,00 €",
           "Restauration de couvertures 210,00 €", "Total HT : 850,00 €", "TVA 20 % : 170,00 €", "Total TTC : 1 020,00 €"]
    write_case(CORPUS, "h20_invoice_blocks", "layout", "Invoice: sender block left and invoice block right on the same lines, client line, 2-column table, right-aligned totals with <br>.",
               "Expected reads the left block then the right block (block order); a row-by-row reading interleaves them (scored as order errors, but both are defensible). Lines broken by <br> are separate lines.",
               body, exp, tcss, checks=[{"type": "sep", "a": "Atelier Martin", "b": "Facture", "kind": "cell"}])


def pairs():
    def pair(pid, note, lblocks, rblocks, css="", diffs=(), lcss=None, rcss=None):
        d = os.path.join(PAIRS, pid)
        os.makedirs(d, exist_ok=True)
        for side, blocks, c in (("left", lblocks, lcss if lcss is not None else css), ("right", rblocks, rcss if rcss is not None else css)):
            body, exp = blocks_to_html(blocks) if isinstance(blocks, list) else blocks
            hp = os.path.join(d, side + ".html")
            with open(hp, "w", encoding="utf-8") as f:
                f.write(page_html(body, c))
            chromium_pdf(hp, os.path.join(d, side + ".pdf"))
            with open(os.path.join(d, side + "_expected.txt"), "w", encoding="utf-8") as f:
                f.write("\n".join(exp) + "\n")
        with open(os.path.join(d, "pair.json"), "w", encoding="utf-8") as f:
            json.dump({"id": pid, "note": note, "expected_differences": list(diffs)}, f, ensure_ascii=False, indent=1)
        print("ok pair", pid, flush=True)

    base = [("h1", "Conditions générales de vente")] + long_doc_blocks(11, 26)
    # 1. one word changed in paragraph 9
    right = list(base)
    i = next(k for k in range(10, len(right)) if right[k][0] == "p")
    old = right[i][1]
    ws = old.split(" ")
    ws[5] = "remarquable"
    right[i] = ("p", " ".join(ws))
    assert right[i][1] != old
    pair("p1_word_changed", "One word changed in one paragraph of a 4-page document with running header and page footer.", base, right, HEADER_CSS,
         diffs=[{"left": old, "right": right[i][1]}])
    # 2. paragraph inserted early (reflows everything after it, page numbers shift)
    ins = "Un nouvel article est ajouté ici : le vendeur s'engage à informer l'acheteur de tout retard de livraison dans un délai de quarante-huit heures, par courrier électronique ou par téléphone."
    right = base[:3] + [("p", ins)] + base[3:]
    pair("p2_para_inserted", "A paragraph inserted on page 1; all following lines and page breaks move.", base, right, HEADER_CSS,
         diffs=[{"left": None, "right": ins}])
    # 3. table cell changed
    rows = [("Article", "Quantité", "Prix")] + [(f"Produit {k}", str(k % 7 + 1), f"{k * 3},50 €") for k in range(1, 30)]
    def tb(rows):
        h = "<table>" + "".join("<tr>" + "".join(f"<td>{esc(c)}</td>" for c in r) + "</tr>" for r in rows) + "</table>"
        return h, [" ".join(r) for r in rows]
    rows2 = list(rows)
    rows2[17] = (rows2[17][0], rows2[17][1], "99,99 €")
    pair("p3_table_cell", "One cell changed in a 30-row table.", tb(rows), tb(rows2), "td{border:1px solid #444;padding:2px 6px} table{border-collapse:collapse}",
         diffs=[{"left": " ".join(rows[17]), "right": " ".join(rows2[17])}])
    # 4. page break moved (no text change)
    body, exp = blocks_to_html(base)
    body2 = body.replace(f"<p>{esc(base[5][1])}</p>", f"<p style='break-before:page'>{esc(base[5][1])}</p>")
    assert body2 != body
    pair("p4_page_break_moved", "Same text; the right file forces a page break before paragraph 5 (all later pages reflow). No real difference.",
         (body, exp), (body2, exp), HEADER_CSS, diffs=[])
    # 5. header text changed only
    pair("p5_header_changed", "Same body; the running header text changes (Version 1 -> Version 2) on every page. No body difference (header = furniture).",
         base, base, diffs=[], lcss="@page { @top-center { content: 'Version 1 du document'; } @bottom-center { content: counter(page); } }",
         rcss="@page { @top-center { content: 'Version 2 du document'; } @bottom-center { content: counter(page); } }")


def big():
    r = rng(2000)
    blocks = []
    n = 0
    while n < 2000 * 11:
        if n % 30 == 0:
            blocks.append(("h2", f"Chapitre {n // 30 + 1}"))
        blocks.append(("p", paragraph(r, 3)))
        n += 1
    body, exp = blocks_to_html(blocks)
    write_case(BIG, "big_2000_pages", "speed", "Long generated document (~2000 pages) with headings and paragraphs, header/footer.", "speed",
               body, exp, HEADER_CSS, furniture=FURN)


if __name__ == "__main__":
    which = sys.argv[4] if len(sys.argv) > 4 else "all"
    if which in ("all", "cases"):
        cases()
    if which in ("all", "pairs"):
        pairs()
    if which in ("all", "big"):
        big()
