"""Tiny hand-written PDF writer for the low-level synthetic cases (no reportlab).

Standard-14 fonts are referenced with WinAnsiEncoding and explicit /Widths taken from the
URW AFM files (Helvetica = Nimbus Sans, Times = Nimbus Roman metrics), so every extractor
gets exact glyph advances and the visual gaps we compute are the gaps the tools see.
"""
import struct
import zlib

AFM_DIR = "/usr/share/libwmf/fonts/"

WINANSI_NAMES = {}
_ascii = {32: "space", 33: "exclam", 34: "quotedbl", 35: "numbersign", 36: "dollar", 37: "percent",
          38: "ampersand", 39: "quotesingle", 40: "parenleft", 41: "parenright", 42: "asterisk",
          43: "plus", 44: "comma", 45: "hyphen", 46: "period", 47: "slash", 58: "colon",
          59: "semicolon", 60: "less", 61: "equal", 62: "greater", 63: "question", 64: "at",
          91: "bracketleft", 92: "backslash", 93: "bracketright", 94: "asciicircum",
          95: "underscore", 96: "grave", 123: "braceleft", 124: "bar", 125: "braceright",
          126: "asciitilde"}
WINANSI_NAMES.update(_ascii)
for i, n in enumerate("zero one two three four five six seven eight nine".split()):
    WINANSI_NAMES[48 + i] = n
for c in range(65, 91):
    WINANSI_NAMES[c] = chr(c)
for c in range(97, 123):
    WINANSI_NAMES[c] = chr(c)
_hi = {128: "Euro", 130: "quotesinglbase", 131: "florin", 132: "quotedblbase", 133: "ellipsis",
       134: "dagger", 135: "daggerdbl", 136: "circumflex", 137: "perthousand", 138: "Scaron",
       139: "guilsinglleft", 140: "OE", 142: "Zcaron", 145: "quoteleft", 146: "quoteright",
       147: "quotedblleft", 148: "quotedblright", 149: "bullet", 150: "endash", 151: "emdash",
       152: "tilde", 153: "trademark", 154: "scaron", 155: "guilsinglright", 156: "oe",
       158: "zcaron", 159: "Ydieresis", 160: "space", 161: "exclamdown", 162: "cent",
       163: "sterling", 164: "currency", 165: "yen", 166: "brokenbar", 167: "section",
       168: "dieresis", 169: "copyright", 170: "ordfeminine", 171: "guillemotleft",
       172: "logicalnot", 173: "hyphen", 174: "registered", 175: "macron", 176: "degree",
       177: "plusminus", 178: "twosuperior", 179: "threesuperior", 180: "acute", 181: "mu",
       182: "paragraph", 183: "periodcentered", 184: "cedilla", 185: "onesuperior",
       186: "ordmasculine", 187: "guillemotright", 188: "onequarter", 189: "onehalf",
       190: "threequarters", 191: "questiondown"}
WINANSI_NAMES.update(_hi)
_up = ("Agrave Aacute Acircumflex Atilde Adieresis Aring AE Ccedilla Egrave Eacute Ecircumflex "
       "Edieresis Igrave Iacute Icircumflex Idieresis Eth Ntilde Ograve Oacute Ocircumflex Otilde "
       "Odieresis multiply Oslash Ugrave Uacute Ucircumflex Udieresis Yacute Thorn germandbls").split()
for i, n in enumerate(_up):
    WINANSI_NAMES[192 + i] = n
_lo = ("agrave aacute acircumflex atilde adieresis aring ae ccedilla egrave eacute ecircumflex "
       "edieresis igrave iacute icircumflex idieresis eth ntilde ograve oacute ocircumflex otilde "
       "odieresis divide oslash ugrave uacute ucircumflex udieresis yacute thorn ydieresis").split()
for i, n in enumerate(_lo):
    WINANSI_NAMES[224 + i] = n


def load_afm(name):
    widths = {}
    with open(AFM_DIR + name + ".afm", encoding="latin-1") as f:
        for line in f:
            if line.startswith("C "):
                parts = [p.strip() for p in line.split(";")]
                wx = n = None
                for p in parts:
                    if p.startswith("WX "):
                        wx = float(p[3:])
                    elif p.startswith("N "):
                        n = p[2:]
                if n is not None:
                    widths[n] = wx
    return widths


STD = {
    "Helvetica": "NimbusSans-Regular", "Helvetica-Bold": "NimbusSans-Bold",
    "Helvetica-Oblique": "NimbusSans-Italic", "Times-Roman": "NimbusRoman-Regular",
    "Times-Italic": "NimbusRoman-Italic", "Times-Bold": "NimbusRoman-Bold",
    "Courier": "NimbusMonoPS-Regular",
}


class StdFont:
    def __init__(self, base):
        self.base = base
        afm = load_afm(STD[base])
        self.w = {}
        for code in range(32, 256):
            n = WINANSI_NAMES.get(code)
            self.w[code] = afm.get(n, 0) if n else 0

    def enc(self, s):
        return s.encode("cp1252")

    def width(self, s, size):
        return sum(self.w.get(b, 0) for b in self.enc(s)) * size / 1000.0

    def obj(self):
        ws = " ".join(str(int(self.w[c])) for c in range(32, 256))
        return (f"<< /Type /Font /Subtype /Type1 /BaseFont /{self.base} /Encoding /WinAnsiEncoding "
                f"/FirstChar 32 /LastChar 255 /Widths [{ws}] >>")


def pdfstr(b):
    """Literal string from bytes."""
    out = bytearray(b"(")
    for x in b:
        if x in (0x28, 0x29, 0x5C):
            out += b"\\" + bytes([x])
        elif x < 32 or x > 126:
            out += b"\\%03o" % x
        else:
            out.append(x)
    out += b")"
    return out.decode("latin-1")


def hexstr(b):
    return "<" + b.hex() + ">"


def wrap(font, text, size, width):
    words = text.split(" ")
    lines, cur = [], []
    for w in words:
        trial = " ".join(cur + [w])
        if cur and font.width(trial, size) > width:
            lines.append(" ".join(cur))
            cur = [w]
        else:
            cur.append(w)
    if cur:
        lines.append(" ".join(cur))
    return lines


class Doc:
    """Objects are strings or (dict_str, bytes) streams; references are 'n 0 R'."""

    def __init__(self):
        self.objs = [None]  # 1-based
        self.pages = []
        self.extra_root = ""
        self.info = None

    def add(self, o):
        self.objs.append(o)
        return len(self.objs) - 1

    def reserve(self):
        return self.add(None)

    def set(self, n, o):
        self.objs[n] = o

    def stream(self, data, extra="", compress=True):
        if isinstance(data, str):
            data = data.encode("latin-1")
        if compress:
            data = zlib.compress(data)
            extra += " /Filter /FlateDecode"
        return self.add((f"<< /Length {len(data)} {extra} >>", data))

    def page(self, contents, resources, mediabox=(0, 0, 595, 842), extra=""):
        """contents: list of stream obj numbers (or one)."""
        if isinstance(contents, int):
            contents = [contents]
        c = contents[0] if len(contents) == 1 else contents
        self.pages.append((c, resources, mediabox, extra))

    def write(self, path):
        pages_n = self.reserve()
        kids = []
        for c, res, mb, extra in self.pages:
            cref = f"{c} 0 R" if isinstance(c, int) else "[" + " ".join(f"{x} 0 R" for x in c) + "]"
            kids.append(self.add(
                f"<< /Type /Page /Parent {pages_n} 0 R /MediaBox [{' '.join(str(x) for x in mb)}] "
                f"/Contents {cref} /Resources {res} {extra} >>"))
        self.page_ids = kids
        self.set(pages_n, f"<< /Type /Pages /Kids [{' '.join(f'{k} 0 R' for k in kids)}] /Count {len(kids)} >>")
        extra_root = self.extra_root(self) if callable(self.extra_root) else self.extra_root
        root = self.add(f"<< /Type /Catalog /Pages {pages_n} 0 R {extra_root} >>")
        info = self.add(self.info or "<< /Producer (pdf-bench pdfgen.py) >>")
        out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
        offsets = [0]
        for i, o in enumerate(self.objs[1:], start=1):
            offsets.append(len(out))
            out += f"{i} 0 obj\n".encode()
            if isinstance(o, tuple):
                out += o[0].encode("latin-1") + b"\nstream\n" + o[1] + b"\nendstream"
            else:
                out += (o or "null").encode("latin-1")
            out += b"\nendobj\n"
        xref = len(out)
        out += f"xref\n0 {len(self.objs)}\n0000000000 65535 f \n".encode()
        for off in offsets[1:]:
            out += f"{off:010d} 00000 n \n".encode()
        out += (f"trailer\n<< /Size {len(self.objs)} /Root {root} 0 R /Info {info} 0 R >>\n"
                f"startxref\n{xref}\n%%EOF\n").encode()
        with open(path, "wb") as f:
            f.write(out)


def std_resources(doc, fonts, extra=""):
    """fonts: dict resname -> StdFont. Returns resource dict string (font objects created once)."""
    parts = []
    for res, f in fonts.items():
        if not hasattr(f, "_objnum") or f._doc is not doc:
            f._objnum = doc.add(f.obj())
            f._doc = doc
        parts.append(f"/{res} {f._objnum} 0 R")
    return f"<< /Font << {' '.join(parts)} >> {extra} >>"


# ---------------------------------------------------------------- TrueType (Type0, Identity-H)
class TTF:
    def __init__(self, path):
        self.data = open(path, "rb").read()
        d = self.data
        num = struct.unpack(">H", d[4:6])[0]
        self.tables = {}
        for i in range(num):
            tag, _, off, ln = struct.unpack(">4sIII", d[12 + 16 * i: 28 + 16 * i])
            self.tables[tag.decode()] = (off, ln)
        off = self.tables["head"][0]
        self.upem = struct.unpack(">H", d[off + 18: off + 20])[0]
        off = self.tables["hhea"][0]
        self.nhm = struct.unpack(">H", d[off + 34: off + 36])[0]
        self.ascent, self.descent = struct.unpack(">hh", d[off + 4: off + 8])
        off = self.tables["hmtx"][0]
        self.adv = [struct.unpack(">H", d[off + 4 * i: off + 4 * i + 2])[0] for i in range(self.nhm)]
        self.cmap = self._cmap()

    def _cmap(self):
        d = self.data
        off = self.tables["cmap"][0]
        n = struct.unpack(">H", d[off + 2: off + 4])[0]
        for i in range(n):
            pid, eid, so = struct.unpack(">HHI", d[off + 4 + 8 * i: off + 12 + 8 * i])
            st = off + so
            fmt = struct.unpack(">H", d[st: st + 2])[0]
            if pid == 3 and eid == 1 and fmt == 4:
                segx2 = struct.unpack(">H", d[st + 6: st + 8])[0]
                seg = segx2 // 2
                ends = struct.unpack(f">{seg}H", d[st + 14: st + 14 + segx2])
                p = st + 16 + segx2
                starts = struct.unpack(f">{seg}H", d[p: p + segx2])
                deltas = struct.unpack(f">{seg}h", d[p + segx2: p + 2 * segx2])
                ro_off = p + 2 * segx2
                ros = struct.unpack(f">{seg}H", d[ro_off: ro_off + segx2])
                m = {}
                for s in range(seg):
                    for c in range(starts[s], ends[s] + 1):
                        if c == 0xFFFF:
                            continue
                        if ros[s] == 0:
                            g = (c + deltas[s]) & 0xFFFF
                        else:
                            a = ro_off + 2 * s + ros[s] + 2 * (c - starts[s])
                            g = struct.unpack(">H", d[a: a + 2])[0]
                            if g:
                                g = (g + deltas[s]) & 0xFFFF
                        if g:
                            m[c] = g
                return m
        raise ValueError("no cmap 3/1/4")

    def gid(self, ch):
        return self.cmap.get(ord(ch), 0)

    def advance(self, gid):
        return self.adv[min(gid, self.nhm - 1)] * 1000.0 / self.upem

    def enc(self, s):
        return b"".join(struct.pack(">H", self.gid(c)) for c in s)

    def width(self, s, size):
        return sum(self.advance(self.gid(c)) for c in s) * size / 1000.0


def type0_font(doc, ttf, name, tounicode=None):
    """tounicode: None (no CMap) or dict gid -> str."""
    ff = doc.stream(ttf.data, f"/Length1 {len(ttf.data)}")
    desc = doc.add(f"<< /Type /FontDescriptor /FontName /{name} /Flags 32 /FontBBox [-200 -300 1200 1000] "
                   f"/ItalicAngle 0 /Ascent {ttf.ascent * 1000 // ttf.upem} /Descent {ttf.descent * 1000 // ttf.upem} "
                   f"/CapHeight 700 /StemV 80 /FontFile2 {ff} 0 R >>")
    used = sorted(set(ttf.cmap.values()))
    w = " ".join(f"{g} [{int(ttf.advance(g))}]" for g in used if g < 3000)
    cid = doc.add(f"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /{name} "
                  f"/CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> "
                  f"/FontDescriptor {desc} 0 R /CIDToGIDMap /Identity /DW 500 /W [{w}] >>")
    tu = ""
    if tounicode is not None:
        lines = []
        items = sorted(tounicode.items())
        for i in range(0, len(items), 100):
            chunk = items[i:i + 100]
            lines.append(f"{len(chunk)} beginbfchar")
            for g, s in chunk:
                lines.append(f"<{g:04X}> <{s.encode('utf-16-be').hex().upper()}>")
            lines.append("endbfchar")
        cmap = ("/CIDInit /ProcSet findresource begin 12 dict begin begincmap "
                "/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def "
                "/CMapName /Adobe-Identity-UCS def /CMapType 2 def 1 begincodespacerange <0000> <FFFF> "
                "endcodespacerange\n" + "\n".join(lines) + "\nendcmap CMapName currentdict /CMap defineresource pop end end")
        tu = f"/ToUnicode {doc.stream(cmap)} 0 R"
    return doc.add(f"<< /Type /Font /Subtype /Type0 /BaseFont /{name} /Encoding /Identity-H "
                   f"/DescendantFonts [{cid} 0 R] {tu} >>")


# ---------------------------------------------------------------- PPM image helpers
def read_ppm(path):
    data = open(path, "rb").read()
    # P6\n[# comment\n]W H\n255\n
    fields, pos = [], 0
    while len(fields) < 4:
        end = data.index(b"\n", pos)
        line = data[pos:end]
        pos = end + 1
        if line.startswith(b"#"):
            continue
        fields += line.split()
    assert fields[0] in (b"P6", b"P5"), fields[0]
    return fields[0], int(fields[1]), int(fields[2]), data[pos:]


def image_xobject(doc, ppm_path):
    kind, w, h, px = read_ppm(ppm_path)
    cs = "/DeviceRGB" if kind == b"P6" else "/DeviceGray"
    return doc.stream(px, f"/Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace {cs} /BitsPerComponent 8"), w, h
