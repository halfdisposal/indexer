//! pdftext — minimal, zero-dependency PDF text extraction.
//!
//! Supports: classic and xref-stream/object-stream PDFs (objects are found by
//! scanning, so broken xref tables are fine), FlateDecode / ASCIIHex / ASCII85
//! filters, ToUnicode CMaps, simple-font /Differences, form XObjects.
//! Not supported: encryption, LZW, OCR/scanned pages, glyph-width based spacing.
#![allow(dead_code)]

use std::collections::HashMap;
use std::rc::Rc;

// ───────────────────────────── inflate (RFC 1950/1951) ─────────────────────────────

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u32,
    cnt: u32,
}

impl<'a> Bits<'a> {
    fn get(&mut self, n: u32) -> Option<u32> {
        while self.cnt < n {
            let b = *self.d.get(self.pos)?;
            self.pos += 1;
            self.buf |= u32::from(b) << self.cnt;
            self.cnt += 8;
        }
        let v = self.buf & ((1u32 << n) - 1);
        self.buf >>= n;
        self.cnt -= n;
        Some(v)
    }
}

struct Huff {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Huff {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[usize::from(l)] += 1;
        }
        count[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..15 {
            offs[i + 1] = offs[i] + count[i];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[usize::from(offs[usize::from(l)])] = s as u16;
                offs[usize::from(l)] += 1;
            }
        }
        Huff { count, symbol }
    }

    fn decode(&self, b: &mut Bits) -> Option<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.get(1)? as i32;
            let count = i32::from(self.count[len]);
            if code - count < first {
                return self.symbol.get((index + (code - first)) as usize).copied();
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        None
    }
}

const LBASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEXT: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DBASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DEXT: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn inflate_blocks(b: &mut Bits, out: &mut Vec<u8>) -> Option<()> {
    loop {
        let last = b.get(1)?;
        match b.get(2)? {
            0 => {
                b.buf = 0;
                b.cnt = 0;
                let h = b.d.get(b.pos..b.pos + 4)?;
                let len = usize::from(h[0]) | (usize::from(h[1]) << 8);
                b.pos += 4;
                out.extend_from_slice(b.d.get(b.pos..b.pos + len)?);
                b.pos += len;
            }
            t @ (1 | 2) => {
                let (lit, dist) = if t == 1 {
                    let mut l = [8u8; 288];
                    l[144..256].fill(9);
                    l[256..280].fill(7);
                    (Huff::new(&l), Huff::new(&[5u8; 30]))
                } else {
                    let hlit = b.get(5)? as usize + 257;
                    let hdist = b.get(5)? as usize + 1;
                    let hclen = b.get(4)? as usize + 4;
                    const ORDER: [usize; 19] =
                        [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut cl = [0u8; 19];
                    for &o in ORDER.iter().take(hclen) {
                        cl[o] = b.get(3)? as u8;
                    }
                    let clh = Huff::new(&cl);
                    let mut lens = vec![0u8; hlit + hdist];
                    let mut i = 0;
                    while i < hlit + hdist {
                        let sym = clh.decode(b)?;
                        let (val, rep) = match sym {
                            0..=15 => (sym as u8, 1),
                            16 => (*lens.get(i.checked_sub(1)?)?, 3 + b.get(2)? as usize),
                            17 => (0, 3 + b.get(3)? as usize),
                            _ => (0, 11 + b.get(7)? as usize),
                        };
                        if i + rep > lens.len() {
                            return None;
                        }
                        lens[i..i + rep].fill(val);
                        i += rep;
                    }
                    (Huff::new(&lens[..hlit]), Huff::new(&lens[hlit..]))
                };
                loop {
                    let sym = lit.decode(b)?;
                    if sym < 256 {
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let i = usize::from(sym) - 257;
                        let len = usize::from(*LBASE.get(i)?)
                            + b.get(u32::from(*LEXT.get(i)?))? as usize;
                        let ds = usize::from(dist.decode(b)?);
                        let d = usize::from(*DBASE.get(ds)?)
                            + b.get(u32::from(*DEXT.get(ds)?))? as usize;
                        if d > out.len() {
                            return None;
                        }
                        for _ in 0..len {
                            let c = out[out.len() - d];
                            out.push(c);
                        }
                    }
                }
            }
            _ => return None,
        }
        if last == 1 {
            return Some(());
        }
    }
}

/// Inflate a zlib stream. Returns whatever was decoded, even if the data is truncated/corrupt.
pub fn inflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 3);
    if data.len() < 2 {
        return out;
    }
    let mut b = Bits { d: data, pos: 2, buf: 0, cnt: 0 };
    let _ = inflate_blocks(&mut b, &mut out);
    out
}

// ───────────────────────────── PDF object model & lexer ─────────────────────────────

type Dict = HashMap<String, Obj>;

#[derive(Debug)]
struct StreamObj {
    dict: Dict,
    raw: Vec<u8>,
}

#[derive(Clone, Debug)]
enum Obj {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Name(String),
    Str(Vec<u8>),
    Arr(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
    Op(String),
    Stream(Rc<StreamObj>),
}

impl Obj {
    fn num(&self) -> Option<f64> {
        match self {
            Obj::Int(i) => Some(*i as f64),
            Obj::Real(r) => Some(*r),
            _ => None,
        }
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}
fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

fn find(d: &[u8], pat: &[u8], from: usize) -> Option<usize> {
    if from >= d.len() {
        return None;
    }
    d[from..].windows(pat.len()).position(|w| w == pat).map(|x| x + from)
}

fn hexval(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|v| v as u8)
}

struct Lexer<'a> {
    d: &'a [u8],
    p: usize,
    refs: bool,
}

impl<'a> Lexer<'a> {
    fn skip_ws(&mut self) {
        while let Some(&b) = self.d.get(self.p) {
            if is_ws(b) {
                self.p += 1;
            } else if b == b'%' {
                while let Some(&c) = self.d.get(self.p) {
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                    self.p += 1;
                }
            } else {
                break;
            }
        }
    }

    fn token(&mut self) -> &'a [u8] {
        let d = self.d;
        let s = self.p;
        while let Some(&b) = d.get(self.p) {
            if is_ws(b) || is_delim(b) {
                break;
            }
            self.p += 1;
        }
        &d[s..self.p]
    }

    fn try_ref(&mut self) -> bool {
        let save = self.p;
        self.skip_ws();
        let t = self.token();
        if !t.is_empty() && t.iter().all(u8::is_ascii_digit) {
            self.skip_ws();
            let after = self.d.get(self.p + 1).map_or(true, |&c| is_ws(c) || is_delim(c));
            if self.d.get(self.p) == Some(&b'R') && after {
                self.p += 1;
                return true;
            }
        }
        self.p = save;
        false
    }

    fn lit_string(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut depth = 1;
        while let Some(&b) = self.d.get(self.p) {
            self.p += 1;
            match b {
                b'(' => {
                    depth += 1;
                    out.push(b);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(b);
                }
                b'\\' => {
                    let Some(&e) = self.d.get(self.p) else { break };
                    self.p += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'0'..=b'7' => {
                            let mut v = u32::from(e - b'0');
                            for _ in 0..2 {
                                match self.d.get(self.p) {
                                    Some(&c @ b'0'..=b'7') => {
                                        v = v * 8 + u32::from(c - b'0');
                                        self.p += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        b'\r' => {
                            if self.d.get(self.p) == Some(&b'\n') {
                                self.p += 1;
                            }
                        }
                        b'\n' => {}
                        other => out.push(other),
                    }
                }
                _ => out.push(b),
            }
        }
        out
    }

    fn hex_string(&mut self) -> Vec<u8> {
        let mut nibbles = Vec::new();
        while let Some(&b) = self.d.get(self.p) {
            self.p += 1;
            if b == b'>' {
                break;
            }
            if let Some(v) = hexval(b) {
                nibbles.push(v);
            }
        }
        if nibbles.len() % 2 == 1 {
            nibbles.push(0);
        }
        nibbles.chunks(2).map(|c| (c[0] << 4) | c[1]).collect()
    }

    fn dict(&mut self) -> Option<Obj> {
        let mut m = Dict::new();
        loop {
            self.skip_ws();
            if self.d[self.p..].starts_with(b">>") {
                self.p += 2;
                break;
            }
            if self.p >= self.d.len() {
                break;
            }
            let Obj::Name(k) = self.parse()? else { continue };
            let v = self.parse()?;
            m.insert(k, v);
        }
        Some(Obj::Dict(m))
    }

    fn parse(&mut self) -> Option<Obj> {
        self.skip_ws();
        let b = *self.d.get(self.p)?;
        match b {
            b'/' => {
                self.p += 1;
                let t = self.token();
                let mut s = Vec::with_capacity(t.len());
                let mut i = 0;
                while i < t.len() {
                    if t[i] == b'#' && i + 2 < t.len() {
                        if let (Some(h), Some(l)) = (hexval(t[i + 1]), hexval(t[i + 2])) {
                            s.push((h << 4) | l);
                            i += 3;
                            continue;
                        }
                    }
                    s.push(t[i]);
                    i += 1;
                }
                Some(Obj::Name(String::from_utf8_lossy(&s).into_owned()))
            }
            b'(' => {
                self.p += 1;
                Some(Obj::Str(self.lit_string()))
            }
            b'<' => {
                if self.d.get(self.p + 1) == Some(&b'<') {
                    self.p += 2;
                    self.dict()
                } else {
                    self.p += 1;
                    Some(Obj::Str(self.hex_string()))
                }
            }
            b'[' => {
                self.p += 1;
                let mut v = Vec::new();
                loop {
                    self.skip_ws();
                    match self.d.get(self.p) {
                        None => break,
                        Some(b']') => {
                            self.p += 1;
                            break;
                        }
                        _ => v.push(self.parse()?),
                    }
                }
                Some(Obj::Arr(v))
            }
            b']' | b'>' | b')' | b'{' | b'}' => {
                self.p += 1;
                Some(Obj::Op((b as char).to_string()))
            }
            _ => {
                let t = self.token();
                let s = std::str::from_utf8(t).unwrap_or("");
                match s {
                    "" => {
                        self.p += 1;
                        Some(Obj::Null)
                    }
                    "true" => Some(Obj::Bool(true)),
                    "false" => Some(Obj::Bool(false)),
                    "null" => Some(Obj::Null),
                    _ => {
                        let numeric = matches!(t[0], b'0'..=b'9' | b'+' | b'-' | b'.');
                        if numeric {
                            if let Ok(i) = s.parse::<i64>() {
                                if self.refs && i >= 0 && self.try_ref() {
                                    return Some(Obj::Ref(i as u32));
                                }
                                return Some(Obj::Int(i));
                            }
                            if let Ok(f) = s.parse::<f64>() {
                                return Some(Obj::Real(f));
                            }
                        }
                        Some(Obj::Op(s.to_string()))
                    }
                }
            }
        }
    }
}

// ───────────────────────────── stream filters ─────────────────────────────

fn ascii_hex_decode(d: &[u8]) -> Vec<u8> {
    let mut lx = Lexer { d, p: 0, refs: false };
    let mut v = lx.hex_string();
    v.shrink_to_fit();
    v
}

fn ascii85_decode(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut grp = [0u32; 5];
    let mut n = 0;
    let d = d.strip_prefix(b"<~").unwrap_or(d);
    for &c in d {
        match c {
            b'~' => break,
            b'z' if n == 0 => out.extend_from_slice(&[0; 4]),
            b'!'..=b'u' => {
                grp[n] = u32::from(c - b'!');
                n += 1;
                if n == 5 {
                    let v = grp.iter().fold(0u32, |a, &g| a.wrapping_mul(85).wrapping_add(g));
                    out.extend_from_slice(&v.to_be_bytes());
                    n = 0;
                }
            }
            _ => {}
        }
    }
    if n > 1 {
        for g in grp.iter_mut().skip(n) {
            *g = 84;
        }
        let v = grp.iter().fold(0u32, |a, &g| a.wrapping_mul(85).wrapping_add(g));
        out.extend_from_slice(&v.to_be_bytes()[..n - 1]);
    }
    out
}

// ───────────────────────────── document ─────────────────────────────

struct Doc {
    objs: HashMap<u32, Obj>,
    null: Obj,
}

fn obj_header(d: &[u8], i: usize) -> Option<u32> {
    let mut q = i;
    while q > 0 && is_ws(d[q - 1]) {
        q -= 1;
    }
    let e = q;
    while q > 0 && d[q - 1].is_ascii_digit() {
        q -= 1;
    }
    if q == e {
        return None;
    }
    let ws = q;
    while q > 0 && is_ws(d[q - 1]) {
        q -= 1;
    }
    if q == ws {
        return None;
    }
    let e2 = q;
    while q > 0 && d[q - 1].is_ascii_digit() {
        q -= 1;
    }
    if q == e2 {
        return None;
    }
    std::str::from_utf8(&d[q..e2]).ok()?.parse().ok()
}

impl Doc {
    fn load(d: &[u8]) -> Doc {
        let mut doc = Doc { objs: HashMap::new(), null: Obj::Null };
        let mut p = 0;
        while let Some(i) = find(d, b"obj", p) {
            p = i + 3;
            let Some(id) = obj_header(d, i) else { continue };
            let mut lx = Lexer { d, p: i + 3, refs: true };
            let Some(o) = lx.parse() else { continue };
            p = lx.p;
            if let Obj::Dict(dict) = o {
                lx.skip_ws();
                if d[lx.p..].starts_with(b"stream") {
                    let mut s = lx.p + 6;
                    if d.get(s) == Some(&b'\r') {
                        s += 1;
                    }
                    if d.get(s) == Some(&b'\n') {
                        s += 1;
                    }
                    let mut end = None;
                    if let Some(Obj::Int(n)) = dict.get("Length") {
                        let e = s.saturating_add(*n as usize);
                        if e <= d.len() {
                            let mut t = Lexer { d, p: e, refs: false };
                            t.skip_ws();
                            if d[t.p..].starts_with(b"endstream") {
                                end = Some(e);
                            }
                        }
                    }
                    let end = end.unwrap_or_else(|| {
                        let mut e = find(d, b"endstream", s).unwrap_or(d.len());
                        if e > s && d[e - 1] == b'\n' {
                            e -= 1;
                        }
                        if e > s && d[e - 1] == b'\r' {
                            e -= 1;
                        }
                        e
                    });
                    let raw = d[s..end].to_vec();
                    doc.objs.insert(id, Obj::Stream(Rc::new(StreamObj { dict, raw })));
                    p = find(d, b"endstream", end).map_or(d.len(), |x| x + 9);
                    continue;
                }
                doc.objs.insert(id, Obj::Dict(dict));
            } else {
                doc.objs.insert(id, o);
            }
        }
        doc.load_object_streams();
        doc
    }

    fn load_object_streams(&mut self) {
        let mut found: Vec<(u32, Obj)> = Vec::new();
        for o in self.objs.values() {
            let Obj::Stream(s) = o else { continue };
            if !matches!(s.dict.get("Type"), Some(Obj::Name(n)) if n == "ObjStm") {
                continue;
            }
            let data = self.decode_stream(s);
            let n = self.get(&s.dict, "N").num().unwrap_or(0.0) as usize;
            let first = self.get(&s.dict, "First").num().unwrap_or(0.0) as usize;
            let mut hdr = Lexer { d: &data, p: 0, refs: false };
            for _ in 0..n {
                let (Some(Obj::Int(num)), Some(Obj::Int(off))) = (hdr.parse(), hdr.parse()) else {
                    break;
                };
                let mut lx = Lexer { d: &data, p: first + off as usize, refs: true };
                if let Some(obj) = lx.parse() {
                    found.push((num as u32, obj));
                }
            }
        }
        for (id, o) in found {
            self.objs.entry(id).or_insert(o);
        }
    }

    fn resolve<'a>(&'a self, o: &'a Obj) -> &'a Obj {
        let mut o = o;
        for _ in 0..32 {
            if let Obj::Ref(id) = o {
                o = self.objs.get(id).unwrap_or(&self.null);
            } else {
                break;
            }
        }
        o
    }

    fn get<'a>(&'a self, d: &'a Dict, k: &str) -> &'a Obj {
        match d.get(k) {
            Some(o) => self.resolve(o),
            None => &self.null,
        }
    }

    fn dict_of<'a>(&'a self, o: &'a Obj) -> Option<&'a Dict> {
        match self.resolve(o) {
            Obj::Dict(d) => Some(d),
            Obj::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }

    fn name_is(&self, d: &Dict, key: &str, val: &str) -> bool {
        matches!(self.get(d, key), Obj::Name(n) if n == val)
    }

    fn decode_stream(&self, s: &StreamObj) -> Vec<u8> {
        let filters: Vec<&str> = match self.get(&s.dict, "Filter") {
            Obj::Name(n) => vec![n.as_str()],
            Obj::Arr(a) => a
                .iter()
                .filter_map(|x| match self.resolve(x) {
                    Obj::Name(n) => Some(n.as_str()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };
        let mut data = s.raw.clone();
        for f in filters {
            data = match f {
                "FlateDecode" | "Fl" => inflate(&data),
                "ASCIIHexDecode" | "AHx" => ascii_hex_decode(&data),
                "ASCII85Decode" | "A85" => ascii85_decode(&data),
                _ => return Vec::new(), // unsupported (LZW, images, ...)
            };
        }
        data
    }

    fn stream_data(&self, o: &Obj) -> Option<Vec<u8>> {
        match self.resolve(o) {
            Obj::Stream(s) => Some(self.decode_stream(s)),
            _ => None,
        }
    }

    // ── page tree ──

    fn collect_pages<'a>(
        &'a self,
        node: &'a Obj,
        res: Option<&'a Obj>,
        out: &mut Vec<(&'a Dict, Option<&'a Obj>)>,
        depth: u32,
    ) {
        if depth > 64 {
            return;
        }
        let Some(d) = self.dict_of(node) else { return };
        let res = d.get("Resources").or(res);
        if let Obj::Arr(kids) = self.get(d, "Kids") {
            for k in kids {
                self.collect_pages(k, res, out, depth + 1);
            }
        } else {
            out.push((d, res));
        }
    }

    fn pages(&self) -> Vec<(&Dict, Option<&Obj>)> {
        let mut ids: Vec<&u32> = self.objs.keys().collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let mut out = Vec::new();
        for id in &ids {
            if let Some(d) = self.dict_of(&self.objs[id]) {
                if self.name_is(d, "Type", "Catalog") {
                    if let Some(root) = d.get("Pages") {
                        self.collect_pages(root, None, &mut out, 0);
                        if !out.is_empty() {
                            return out;
                        }
                    }
                }
            }
        }
        // Fallback: no usable catalog — take every /Type /Page in object order.
        ids.reverse();
        for id in ids {
            if let Some(d) = self.dict_of(&self.objs[id]) {
                if self.name_is(d, "Type", "Page") {
                    out.push((d, d.get("Resources")));
                }
            }
        }
        out
    }
}

// ───────────────────────────── fonts ─────────────────────────────

struct FontInfo {
    cmap: HashMap<u32, String>,
    width: usize,
    enc: HashMap<u8, char>,
}

fn be(b: &[u8]) -> u32 {
    b.iter().fold(0u32, |a, &x| (a << 8) | u32::from(x))
}

fn utf16(b: &[u8]) -> String {
    if b.len() == 1 {
        return (b[0] as char).to_string();
    }
    let units = b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]]));
    char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect()
}

fn winansi(b: u8) -> char {
    match b {
        0x80 => '€',
        0x85 => '…',
        0x91 => '‘',
        0x92 => '’',
        0x93 => '“',
        0x94 => '”',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x99 => '™',
        0..=0x1f => ' ',
        _ => b as char,
    }
}

fn glyph_char(n: &str) -> Option<char> {
    let mut it = n.chars();
    if let (Some(c), None) = (it.next(), it.next()) {
        return Some(c);
    }
    if let Some(h) = n.strip_prefix("uni") {
        if let Ok(v) = u32::from_str_radix(h, 16) {
            return char::from_u32(v);
        }
    }
    Some(match n {
        "space" | "nbspace" => ' ',
        "hyphen" | "minus" => '-',
        "period" => '.',
        "comma" => ',',
        "colon" => ':',
        "semicolon" => ';',
        "exclam" => '!',
        "question" => '?',
        "quotesingle" => '\'',
        "quotedbl" => '"',
        "quoteleft" => '‘',
        "quoteright" => '’',
        "quotedblleft" => '“',
        "quotedblright" => '”',
        "endash" => '–',
        "emdash" => '—',
        "bullet" => '•',
        "ellipsis" => '…',
        "ff" => 'ﬀ',
        "ffi" => 'ﬃ',
        "ffl" => 'ﬄ',
        "fi" => 'ﬁ',
        "fl" => 'ﬂ',
        "parenleft" => '(',
        "parenright" => ')',
        "bracketleft" => '[',
        "bracketright" => ']',
        "slash" => '/',
        "plus" => '+',
        "equal" => '=',
        "asterisk" => '*',
        "ampersand" => '&',
        "percent" => '%',
        "dollar" => '$',
        "numbersign" => '#',
        "at" => '@',
        "underscore" => '_',
        "zero" => '0',
        "one" => '1',
        "two" => '2',
        "three" => '3',
        "four" => '4',
        "five" => '5',
        "six" => '6',
        "seven" => '7',
        "eight" => '8',
        "nine" => '9',
        _ => return None,
    })
}

/// Returns (code → text map, optional code width in bytes from codespacerange).
fn parse_cmap(data: &[u8]) -> (HashMap<u32, String>, Option<usize>) {
    let mut map = HashMap::new();
    let mut width = None;
    let mut lx = Lexer { d: data, p: 0, refs: false };
    let mut st: Vec<Obj> = Vec::new();
    while let Some(o) = lx.parse() {
        let Obj::Op(op) = &o else {
            st.push(o);
            continue;
        };
        match op.as_str() {
            "begincodespacerange" | "beginbfchar" | "beginbfrange" => st.clear(),
            "endcodespacerange" => {
                if let Some(Obj::Str(s)) = st.first() {
                    width = Some(s.len().max(1));
                }
                st.clear();
            }
            "endbfchar" => {
                for pair in st.chunks(2) {
                    if let [Obj::Str(a), Obj::Str(b)] = pair {
                        map.insert(be(a), utf16(b));
                    }
                }
                st.clear();
            }
            "endbfrange" => {
                for t in st.chunks(3) {
                    let [Obj::Str(lo), Obj::Str(hi), dst] = t else { continue };
                    let (lo, hi) = (be(lo), be(hi));
                    if hi < lo || hi - lo > 0xFFFF {
                        continue;
                    }
                    match dst {
                        Obj::Str(b) => {
                            let base: Vec<char> = utf16(b).chars().collect();
                            for (i, code) in (lo..=hi).enumerate() {
                                let mut s = base.clone();
                                if let Some(l) = s.last_mut() {
                                    *l = char::from_u32(*l as u32 + i as u32).unwrap_or('\u{FFFD}');
                                }
                                map.insert(code, s.into_iter().collect());
                            }
                        }
                        Obj::Arr(a) => {
                            for (i, x) in a.iter().enumerate() {
                                if let Obj::Str(b) = x {
                                    map.insert(lo + i as u32, utf16(b));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                st.clear();
            }
            _ => {}
        }
    }
    (map, width)
}

impl FontInfo {
    fn decode(&self, s: &[u8]) -> String {
        let mut out = String::new();
        let w = self.width.max(1);
        let simple = |b: u8| self.enc.get(&b).copied().unwrap_or_else(|| winansi(b));
        if !self.cmap.is_empty() {
            for ch in s.chunks(w) {
                match self.cmap.get(&be(ch)) {
                    Some(t) => out.push_str(t),
                    None if w == 1 => out.push(simple(ch[0])),
                    None => {}
                }
            }
        } else if w == 1 {
            out.extend(s.iter().map(|&b| simple(b)));
        }
        out
    }
}

impl Doc {
    fn load_font(&self, fd: &Dict) -> FontInfo {
        let mut f = FontInfo {
            cmap: HashMap::new(),
            width: if self.name_is(fd, "Subtype", "Type0") { 2 } else { 1 },
            enc: HashMap::new(),
        };
        if let Some(data) = fd.get("ToUnicode").and_then(|o| self.stream_data(o)) {
            let (map, w) = parse_cmap(&data);
            f.cmap = map;
            if let Some(w) = w {
                f.width = w;
            }
        }
        if let Some(ed) = self.dict_of(self.get(fd, "Encoding")) {
            if let Obj::Arr(diffs) = self.get(ed, "Differences") {
                let mut code = 0i64;
                for x in diffs {
                    match self.resolve(x) {
                        Obj::Int(i) => code = *i,
                        Obj::Name(n) => {
                            if let (Some(c), Ok(b)) = (glyph_char(n), u8::try_from(code)) {
                                f.enc.insert(b, c);
                            }
                            code += 1;
                        }
                        _ => {}
                    }
                }
            }
        }
        f
    }

    fn find_font<'a>(
        &'a self,
        res: Option<&'a Obj>,
        name: &str,
        cache: &mut HashMap<u32, Rc<FontInfo>>,
    ) -> Option<Rc<FontInfo>> {
        let rd = self.dict_of(res?)?;
        let fonts = self.dict_of(self.get(rd, "Font"))?;
        let e = fonts.get(name)?;
        if let Obj::Ref(id) = e {
            if let Some(f) = cache.get(id) {
                return Some(Rc::clone(f));
            }
            let f = Rc::new(self.load_font(self.dict_of(e)?));
            cache.insert(*id, Rc::clone(&f));
            Some(f)
        } else {
            Some(Rc::new(self.load_font(self.dict_of(e)?)))
        }
    }
}

// ───────────────────────────── content-stream interpreter ─────────────────────────────

struct Ctx {
    font: Option<Rc<FontInfo>>,
    size: f64,
    lx: f64,
    ly: f64,
    lead: f64,
    last_y: Option<f64>,
    moved: bool,
}

impl Ctx {
    fn emit(&mut self, text: &str, out: &mut String) {
        if text.is_empty() {
            return;
        }
        let thr = (self.size * 0.3).max(0.5);
        match self.last_y {
            Some(y) if (y - self.ly).abs() > thr => {
                while out.ends_with(' ') {
                    out.pop();
                }
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            _ => {
                if self.moved
                    && !out.is_empty()
                    && !out.ends_with(char::is_whitespace)
                    && !text.starts_with(char::is_whitespace)
                {
                    out.push(' ');
                }
            }
        }
        self.moved = false;
        self.last_y = Some(self.ly);
        out.push_str(text);
    }

    fn show(&mut self, s: &[u8], out: &mut String) {
        let text = match &self.font {
            Some(f) => f.decode(s),
            None => s.iter().map(|&b| winansi(b)).collect(),
        };
        self.emit(&text, out);
    }
}

fn last_nums(st: &[Obj], n: usize) -> Option<Vec<f64>> {
    if st.len() < n {
        return None;
    }
    st[st.len() - n..].iter().map(Obj::num).collect()
}

impl Doc {
    fn run<'a>(
        &'a self,
        data: &[u8],
        res: Option<&'a Obj>,
        ctx: &mut Ctx,
        out: &mut String,
        cache: &mut HashMap<u32, Rc<FontInfo>>,
        depth: u32,
    ) {
        let mut lx = Lexer { d: data, p: 0, refs: false };
        let mut st: Vec<Obj> = Vec::new();
        while let Some(o) = lx.parse() {
            let op = match o {
                Obj::Op(op) => op,
                other => {
                    if st.len() < 512 {
                        st.push(other);
                    }
                    continue;
                }
            };
            match op.as_str() {
                "BT" => {
                    ctx.lx = 0.0;
                    ctx.ly = 0.0;
                    ctx.moved = true;
                }
                "Tf" => {
                    if let [Obj::Name(n), sz] = &st[st.len().saturating_sub(2)..] {
                        ctx.size = sz.num().unwrap_or(1.0).abs();
                        ctx.font = self.find_font(res, n, cache);
                    }
                }
                "Td" | "TD" => {
                    if let Some(v) = last_nums(&st, 2) {
                        ctx.lx += v[0];
                        ctx.ly += v[1];
                        ctx.moved = true;
                        if op == "TD" {
                            ctx.lead = -v[1];
                        }
                    }
                }
                "Tm" => {
                    if let Some(v) = last_nums(&st, 6) {
                        ctx.lx = v[4];
                        ctx.ly = v[5];
                        ctx.moved = true;
                    }
                }
                "T*" => {
                    ctx.ly -= ctx.lead;
                    ctx.moved = true;
                }
                "TL" => {
                    if let Some(v) = last_nums(&st, 1) {
                        ctx.lead = v[0];
                    }
                }
                "Tj" => {
                    if let Some(Obj::Str(s)) = st.last() {
                        ctx.show(s, out);
                    }
                }
                "'" | "\"" => {
                    ctx.ly -= ctx.lead;
                    ctx.moved = true;
                    if let Some(Obj::Str(s)) = st.last() {
                        ctx.show(s, out);
                    }
                }
                "TJ" => {
                    if let Some(Obj::Arr(a)) = st.last() {
                        for x in a {
                            match x {
                                Obj::Str(s) => ctx.show(s, out),
                                n => {
                                    if n.num().map_or(false, |v| v < -200.0) {
                                        ctx.moved = true;
                                    }
                                }
                            }
                        }
                    }
                }
                "Do" => {
                    if let (Some(Obj::Name(n)), true) = (st.last(), depth < 8) {
                        self.run_xobject(res, n, ctx, out, cache, depth);
                    }
                }
                "BI" => {
                    // skip inline image data up to the "EI" keyword
                    let mut from = lx.p;
                    while let Some(i) = find(data, b"EI", from) {
                        let before = i == 0 || is_ws(data[i - 1]);
                        let after = data.get(i + 2).map_or(true, |&c| is_ws(c));
                        if before && after {
                            lx.p = i + 2;
                            break;
                        }
                        from = i + 2;
                    }
                }
                _ => {}
            }
            st.clear();
        }
    }

    fn run_xobject<'a>(
        &'a self,
        res: Option<&'a Obj>,
        name: &str,
        ctx: &mut Ctx,
        out: &mut String,
        cache: &mut HashMap<u32, Rc<FontInfo>>,
        depth: u32,
    ) {
        let Some(rd) = res.and_then(|r| self.dict_of(r)) else { return };
        let Some(xd) = self.dict_of(self.get(rd, "XObject")) else { return };
        let Some(x) = xd.get(name) else { return };
        if let Obj::Stream(s) = self.resolve(x) {
            if self.name_is(&s.dict, "Subtype", "Form") {
                let data = self.decode_stream(s);
                let r = s.dict.get("Resources").or(res);
                self.run(&data, r, ctx, out, cache, depth + 1);
            }
        }
    }
}

// ───────────────────────────── public API ─────────────────────────────

/// Extract text from every page of a PDF. One `String` per page.
pub fn extract_pages(data: &[u8]) -> Result<Vec<String>, String> {
    if find(&data[..data.len().min(1024)], b"%PDF-", 0).is_none() {
        return Err("not a PDF file".into());
    }
    if find(data, b"/Encrypt", 0).is_some() {
        return Err("encrypted PDFs are not supported".into());
    }
    let doc = Doc::load(data);
    let mut cache = HashMap::new();
    let mut result = Vec::new();
    for (page, res) in doc.pages() {
        let mut content = Vec::new();
        match doc.get(page, "Contents") {
            Obj::Stream(s) => content = doc.decode_stream(s),
            Obj::Arr(a) => {
                for x in a {
                    if let Some(d) = doc.stream_data(x) {
                        content.extend_from_slice(&d);
                        content.push(b'\n');
                    }
                }
            }
            _ => {}
        }
        let mut ctx = Ctx {
            font: None,
            size: 1.0,
            lx: 0.0,
            ly: 0.0,
            lead: 0.0,
            last_y: None,
            moved: false,
        };
        let mut text = String::new();
        doc.run(&content, res, &mut ctx, &mut text, &mut cache, 0);
        result.push(text);
    }
    Ok(result)
}

/// Convenience: all pages joined by form feeds (like `pdftotext`).
pub fn extract_text(data: &[u8]) -> Result<String, String> {
    Ok(extract_pages(data)?.join("\n\x0c"))
}
