//! x86-64 assembler for the Auto Assembler instruction subset.
//!
//! Tables assume an assembler is present. Without one, an AA
//! script can allocate memory and register symbols but never actually injects code,
//! which leaves hook-populated symbols (e.g. `_pPos`) pointing at zeroed memory.
//!
//! Numeric literals follow table conventions: **hex by default**, `#` prefixes a
//! decimal literal, `0x`/`$` are explicit hex.
//!
//! Branches and RIP-relative operands always encode with 32-bit displacements so
//! an instruction's length never depends on the value of a symbol. That keeps
//! sizes stable between the layout pass and the emit pass.
//!
//! [`Mode::Bits32`] encodes for a 32-bit (WoW64) game instead. The syntax is
//! the same; what changes is that no REX prefix exists, an absolute operand is
//! a plain disp32 rather than RIP-relative, and every branch is reachable with
//! a rel32 because addresses wrap at 4GB. A 64-bit register in 32-bit mode is
//! an error, never a silent re-encoding.

use anyhow::{bail, Result};

/// Resolves a symbol/label name to an absolute address.
pub type Resolver<'a> = &'a dyn Fn(&str) -> Option<u64>;

/// Supplies an 8-byte slot holding `target`, for calls/jumps that cannot be
/// reached with a 32-bit relative displacement.
///
/// Windows APIs sit hundreds of gigabytes from a game module, so
/// `call kernel32.Sleep` from injected code has to go through a pointer.
pub type FarSlots<'a> = &'a dyn Fn(u64) -> Option<u64>;

/// Bitness of the process the code is assembled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// IA-32: the encoding a WoW64 game runs.
    Bits32,
    /// x86-64, the default and the only mode most tables need.
    #[default]
    Bits64,
}

impl Mode {
    pub fn for_target(is_64bit: bool) -> Self {
        if is_64bit {
            Mode::Bits64
        } else {
            Mode::Bits32
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegKind {
    G8,
    /// 8-bit register that requires a REX prefix to encode (spl/bpl/sil/dil, r8b+).
    G8Rex,
    /// ah/ch/dh/bh - cannot coexist with a REX prefix.
    G8High,
    G16,
    G32,
    G64,
    Xmm,
}

#[derive(Debug, Clone, Copy)]
pub struct Reg {
    pub num: u8,
    pub kind: RegKind,
}

impl Reg {
    fn size(&self) -> u8 {
        match self.kind {
            RegKind::G8 | RegKind::G8Rex | RegKind::G8High => 1,
            RegKind::G16 => 2,
            RegKind::G32 => 4,
            RegKind::G64 => 8,
            RegKind::Xmm => 16,
        }
    }
    fn is_gpr(&self) -> bool {
        self.kind != RegKind::Xmm
    }
}

#[derive(Debug, Clone, Default)]
pub struct Mem {
    pub base: Option<Reg>,
    pub index: Option<Reg>,
    pub scale: u8,
    pub disp: i64,
    /// Absolute target address when there is no base/index register.
    pub abs: Option<u64>,
    /// Explicit size from a `byte`/`word`/`dword`/`qword` specifier, in bytes.
    pub size: Option<u8>,
    /// Segment-override prefix byte, for `fs:[30]`-style thread-local access.
    pub seg: Option<u8>,
}

#[derive(Debug, Clone)]
pub enum Op {
    R(Reg),
    M(Mem),
    I(i64),
}

// ---------------------------------------------------------------------------
// Instruction buffer
// ---------------------------------------------------------------------------

/// VEX prefix fields for AVX encodings.
#[derive(Clone, Copy)]
struct Vex {
    /// Implied legacy prefix: 0=none, 1=66, 2=F3, 3=F2.
    pp: u8,
    /// Opcode map: 1=0F, 2=0F38, 3=0F3A.
    mmmmm: u8,
    w: bool,
    l: bool,
    /// Second source register. The field is written inverted, so 0 here
    /// produces the all-ones "unused" encoding.
    vvvv: u8,
}

#[derive(Default)]
struct Insn {
    vex: Option<Vex>,
    prefixes: Vec<u8>,
    rex_w: bool,
    rex_r: bool,
    rex_x: bool,
    rex_b: bool,
    need_rex: bool,
    /// Set when an operand uses ah/ch/dh/bh, which forbids a REX prefix.
    forbid_rex: bool,
    opcode: Vec<u8>,
    modrm: Option<u8>,
    sib: Option<u8>,
    disp: Vec<u8>,
    imm: Vec<u8>,
    rip_rel: bool,
    rip_target: u64,
    mode: Mode,
}

impl Insn {
    fn emit(&self, addr: u64) -> Result<Vec<u8>> {
        // VEX replaces both the legacy prefixes and REX.
        if let Some(vex) = self.vex {
            if self.mode == Mode::Bits32 && (self.rex_r || self.rex_x || self.rex_b || vex.w) {
                bail!("xmm8-15 and 64-bit operands do not exist in 32-bit mode");
            }
            let mut v = Vec::new();
            let need_three = self.rex_x || self.rex_b || vex.mmmmm != 1 || vex.w;
            let bit = |set: bool| if set { 0u8 } else { 1u8 }; // VEX stores these inverted
            if need_three {
                v.push(0xC4);
                v.push((bit(self.rex_r) << 7) | (bit(self.rex_x) << 6) | (bit(self.rex_b) << 5) | vex.mmmmm);
                v.push(((vex.w as u8) << 7) | ((!vex.vvvv & 0xF) << 3) | ((vex.l as u8) << 2) | vex.pp);
            } else {
                v.push(0xC5);
                v.push((bit(self.rex_r) << 7) | ((!vex.vvvv & 0xF) << 3) | ((vex.l as u8) << 2) | vex.pp);
            }
            // mmmmm already encodes the 0F escape, so drop the stored 0x0F.
            let opcode = if self.opcode.first() == Some(&0x0F) {
                &self.opcode[1..]
            } else {
                &self.opcode[..]
            };
            v.extend_from_slice(opcode);
            if let Some(m) = self.modrm {
                v.push(m);
            }
            if let Some(s) = self.sib {
                v.push(s);
            }
            let disp_pos = v.len();
            v.extend_from_slice(&self.disp);
            v.extend_from_slice(&self.imm);
            if self.rip_rel {
                let next = addr.wrapping_add(v.len() as u64);
                let d = self.rip_target.wrapping_sub(next) as i64;
                if d < i32::MIN as i64 || d > i32::MAX as i64 {
                    bail!("RIP-relative target {:#X} is out of ±2GB range", self.rip_target);
                }
                v[disp_pos..disp_pos + 4].copy_from_slice(&(d as i32).to_le_bytes());
            }
            return Ok(v);
        }

        let has_rex = self.rex_w || self.rex_r || self.rex_x || self.rex_b || self.need_rex;
        if has_rex && self.forbid_rex {
            bail!("cannot encode ah/ch/dh/bh together with an extended register or 64-bit operand");
        }
        if has_rex && self.mode == Mode::Bits32 {
            bail!("64-bit registers, r8-r15 and spl/bpl/sil/dil do not exist in 32-bit mode");
        }

        let mut v = self.prefixes.clone();
        if has_rex {
            let mut r = 0x40u8;
            if self.rex_w {
                r |= 0b1000;
            }
            if self.rex_r {
                r |= 0b0100;
            }
            if self.rex_x {
                r |= 0b0010;
            }
            if self.rex_b {
                r |= 0b0001;
            }
            v.push(r);
        }
        v.extend_from_slice(&self.opcode);
        if let Some(m) = self.modrm {
            v.push(m);
        }
        if let Some(s) = self.sib {
            v.push(s);
        }
        let disp_pos = v.len();
        v.extend_from_slice(&self.disp);
        v.extend_from_slice(&self.imm);

        if self.rip_rel {
            // disp is relative to the end of the whole instruction.
            let next = addr.wrapping_add(v.len() as u64);
            let d = self.rip_target.wrapping_sub(next) as i64;
            if d < i32::MIN as i64 || d > i32::MAX as i64 {
                bail!(
                    "RIP-relative target {:#X} is out of ±2GB range from {:#X}",
                    self.rip_target,
                    addr
                );
            }
            v[disp_pos..disp_pos + 4].copy_from_slice(&(d as i32).to_le_bytes());
        }
        Ok(v)
    }

    /// Encodes a register operand into the ModRM.reg field.
    fn set_reg(&mut self, r: Reg) {
        if r.num >= 8 {
            self.rex_r = true;
        }
        if r.kind == RegKind::G8Rex {
            self.need_rex = true;
        }
        if r.kind == RegKind::G8High {
            self.forbid_rex = true;
        }
        let cur = self.modrm.unwrap_or(0);
        self.modrm = Some((cur & 0b1100_0111) | ((r.num & 7) << 3));
    }

    fn set_reg_raw(&mut self, field: u8) {
        let cur = self.modrm.unwrap_or(0);
        self.modrm = Some((cur & 0b1100_0111) | ((field & 7) << 3));
    }

    /// Encodes an r/m operand (register form).
    fn set_rm_reg(&mut self, r: Reg) {
        if r.num >= 8 {
            self.rex_b = true;
        }
        if r.kind == RegKind::G8Rex {
            self.need_rex = true;
        }
        if r.kind == RegKind::G8High {
            self.forbid_rex = true;
        }
        let cur = self.modrm.unwrap_or(0);
        self.modrm = Some((cur & 0b0011_1000) | 0b1100_0000 | (r.num & 7));
    }

    /// Encodes an r/m operand (memory form), emitting ModRM/SIB/disp.
    fn set_rm_mem(&mut self, m: &Mem) -> Result<()> {
        // Segment overrides come before every other prefix.
        if let Some(seg) = m.seg {
            self.prefixes.insert(0, seg);
        }
        let reg_field = self.modrm.unwrap_or(0) & 0b0011_1000;

        // Segment-relative with no registers is an absolute offset within the
        // segment, so it uses SIB-with-no-base rather than RIP-relative.
        if m.seg.is_some() && m.base.is_none() && m.index.is_none() {
            self.modrm = Some(reg_field | 0b0000_0100); // mod=00, rm=100 (SIB)
            self.sib = Some(0b00_100_101); // no index, no base -> disp32
            self.disp = (m.disp as i32).to_le_bytes().to_vec();
            return Ok(());
        }

        // No base and no index: absolute address, encoded RIP-relative.
        if m.base.is_none() && m.index.is_none() {
            let target = m.abs.unwrap_or(m.disp as u64);
            self.modrm = Some(reg_field | 0b0000_0101); // mod=00, rm=101 => RIP+disp32
            if self.mode == Mode::Bits32 {
                // The same ModRM is a plain disp32 in 32-bit mode.
                if target > u32::MAX as u64 {
                    bail!("address {:#X} does not fit a 32-bit process", target);
                }
                self.disp = (target as u32).to_le_bytes().to_vec();
                return Ok(());
            }
            self.disp = vec![0, 0, 0, 0];
            self.rip_rel = true;
            self.rip_target = target;
            return Ok(());
        }

        if self.mode == Mode::Bits32 {
            let is_64 = |r: Option<Reg>| matches!(r, Some(r) if r.kind == RegKind::G64);
            if is_64(m.base) || is_64(m.index) {
                bail!("64-bit address registers do not exist in 32-bit mode");
            }
        }

        if let Some(ix) = m.index {
            if ix.num == 4 && ix.kind != RegKind::Xmm {
                bail!("rsp cannot be used as an index register");
            }
            if ix.num >= 8 {
                self.rex_x = true;
            }
        }
        if let Some(b) = m.base {
            if b.num >= 8 {
                self.rex_b = true;
            }
        }

        let scale_bits = match m.scale {
            0 | 1 => 0u8,
            2 => 1,
            4 => 2,
            8 => 3,
            s => bail!("invalid index scale {}", s),
        };

        let base = m.base;
        let needs_sib = m.index.is_some() || matches!(base, Some(b) if b.num & 7 == 4);

        // rbp/r13 have no mod=00 form; they always need at least a disp8.
        let base_is_bp = matches!(base, Some(b) if b.num & 7 == 5);
        let no_base = base.is_none();

        let mod_bits = if no_base {
            0b00 // SIB with base=101 and mod=00 means disp32, no base
        } else if m.disp == 0 && !base_is_bp {
            0b00
        } else if (-128..=127).contains(&m.disp) {
            0b01
        } else {
            0b10
        };

        if needs_sib || no_base {
            self.modrm = Some(reg_field | (mod_bits << 6) | 0b100);
            let index_bits = m.index.map(|i| i.num & 7).unwrap_or(0b100);
            let base_bits = base.map(|b| b.num & 7).unwrap_or(0b101);
            self.sib = Some((scale_bits << 6) | (index_bits << 3) | base_bits);
        } else {
            let b = base.expect("base present");
            self.modrm = Some(reg_field | (mod_bits << 6) | (b.num & 7));
        }

        match mod_bits {
            0b01 => self.disp = vec![m.disp as i8 as u8],
            0b10 => self.disp = (m.disp as i32).to_le_bytes().to_vec(),
            0b00 if no_base => self.disp = (m.disp as i32).to_le_bytes().to_vec(),
            _ => {}
        }
        Ok(())
    }

    fn apply_operand_size(&mut self, size: u8) {
        match size {
            2 => self.prefixes.push(0x66),
            8 => self.rex_w = true,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

const R64: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];
const R32: [&str; 16] = [
    "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d",
    "r13d", "r14d", "r15d",
];
const R16: [&str; 16] = [
    "ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w",
    "r14w", "r15w",
];
const R8: [&str; 16] = [
    "al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b",
    "r13b", "r14b", "r15b",
];
/// Tables write the low byte of the extended registers as `r8l`, not `r8b`.
const R8_ALT: [&str; 8] = ["r8l", "r9l", "r10l", "r11l", "r12l", "r13l", "r14l", "r15l"];
const R8H: [&str; 4] = ["ah", "ch", "dh", "bh"];

pub fn parse_reg(s: &str) -> Option<Reg> {
    let t = s.trim().to_lowercase();
    if let Some(i) = R64.iter().position(|r| *r == t) {
        return Some(Reg { num: i as u8, kind: RegKind::G64 });
    }
    if let Some(i) = R32.iter().position(|r| *r == t) {
        return Some(Reg { num: i as u8, kind: RegKind::G32 });
    }
    if let Some(i) = R16.iter().position(|r| *r == t) {
        return Some(Reg { num: i as u8, kind: RegKind::G16 });
    }
    if let Some(i) = R8.iter().position(|r| *r == t) {
        let kind = if i >= 4 { RegKind::G8Rex } else { RegKind::G8 };
        return Some(Reg { num: i as u8, kind });
    }
    if let Some(i) = R8_ALT.iter().position(|r| *r == t) {
        return Some(Reg { num: i as u8 + 8, kind: RegKind::G8Rex });
    }
    if let Some(i) = R8H.iter().position(|r| *r == t) {
        return Some(Reg { num: i as u8 + 4, kind: RegKind::G8High });
    }
    if let Some(rest) = t.strip_prefix("xmm") {
        if let Ok(n) = rest.parse::<u8>() {
            if n < 16 {
                return Some(Reg { num: n, kind: RegKind::Xmm });
            }
        }
    }
    None
}

/// Parses a numeric literal using table conventions (hex by default, `#` = decimal).
///
/// Also handles the typed constants, `(float)1.0` and `(double)2.5`, which
/// yield the IEEE-754 bit pattern. Tables use these in data directives
/// (`dd (float)1.0`) and immediates.
pub fn parse_number(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }

    // The five casts. Longer names first:
    // `(double32l)` must not be mistaken for `(double)`.
    let lower = t.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("(double32l)") {
        // Low half of the double's bit pattern.
        return rest.trim().parse::<f64>().ok().map(|f| (f.to_bits() & 0xFFFF_FFFF) as i64);
    }
    if let Some(rest) = lower.strip_prefix("(double32h)") {
        // High half.
        return rest.trim().parse::<f64>().ok().map(|f| (f.to_bits() >> 32) as i64);
    }
    if let Some(rest) = lower.strip_prefix("(float)") {
        return rest.trim().parse::<f32>().ok().map(|f| f.to_bits() as i64);
    }
    if let Some(rest) = lower.strip_prefix("(double)") {
        return rest.trim().parse::<f64>().ok().map(|f| f.to_bits() as i64);
    }
    if let Some(rest) = lower.strip_prefix("(int)") {
        // This uses `StrToQWordEx`, so it is **decimal** - the one place a bare
        // number in an AA script is not hex. `(int)100` is 100, not 0x100, and
        // getting that backwards writes a plausible-looking wrong value rather
        // than failing.
        let r = rest.trim();
        return r
            .parse::<i64>()
            .ok()
            .or_else(|| r.parse::<u64>().ok().map(|v| v as i64));
    }
    if let Some(r) = t.strip_prefix('-') {
        return parse_number(r).map(|v| -v);
    }
    if let Some(r) = t.strip_prefix('#') {
        let r = r.trim();
        // "#500" is decimal; "#x1F" is not valid syntax, so plain decimal only.
        return r.parse::<i64>().ok();
    }
    if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(r, 16).ok();
    }
    if let Some(r) = t.strip_prefix('$') {
        return i64::from_str_radix(r, 16).ok();
    }
    // Hex by default. u64 first so values like FFFFFFFFFFFFFFFF parse.
    if let Ok(v) = u64::from_str_radix(t, 16) {
        return Some(v as i64);
    }
    None
}

/// Drops a NASM-style `rel` marker: `[rel Game.exe+123]`. Absolute operands
/// are already encoded RIP-relative, so the keyword adds nothing.
fn strip_rel(s: &str) -> &str {
    let t = s.trim_start();
    for kw in ["rel ", "REL "] {
        if let Some(rest) = t.strip_prefix(kw) {
            return rest.trim_start();
        }
    }
    t
}

fn strip_size_specifier(s: &str) -> (Option<u8>, &str) {
    let t = s.trim_start();
    let lower = t.to_lowercase();
    for (name, sz) in [
        ("qword ptr", 8u8),
        ("dword ptr", 4),
        ("word ptr", 2),
        ("byte ptr", 1),
        ("qword", 8),
        ("dword", 4),
        ("word", 2),
        ("byte", 1),
    ] {
        if lower.starts_with(name) {
            let rest = t[name.len()..].trim_start();
            if rest.starts_with('[') {
                return (Some(sz), rest);
            }
        }
    }
    (None, t)
}

/// Splits an expression on top-level `+`/`-`, keeping the sign with each term.
fn split_terms(expr: &str) -> Vec<(i8, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut sign = 1i8;
    for ch in expr.chars() {
        match ch {
            '+' | '-' => {
                if !cur.trim().is_empty() {
                    out.push((sign, cur.trim().to_string()));
                }
                cur.clear();
                sign = if ch == '-' { -1 } else { 1 };
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push((sign, cur.trim().to_string()));
    }
    out
}

/// Strips a `fs:` / `gs:` / ... prefix, returning its override byte.
fn strip_segment(s: &str) -> (Option<u8>, &str) {
    let t = s.trim_start();
    for (name, byte) in [
        ("fs:", 0x64u8),
        ("gs:", 0x65),
        ("es:", 0x26),
        ("cs:", 0x2E),
        ("ss:", 0x36),
        ("ds:", 0x3E),
    ] {
        if t.len() > name.len() && t[..name.len()].eq_ignore_ascii_case(name) {
            return (Some(byte), t[name.len()..].trim_start());
        }
    }
    (None, t)
}

fn parse_mem(s: &str, resolve: Resolver) -> Result<Mem> {
    let (size, body) = strip_size_specifier(s);
    let (seg, body) = strip_segment(body);
    let body = body.trim();
    // `fs:00000030` without brackets is still a memory reference.
    let inner: &str = if body.starts_with('[') && body.ends_with(']') {
        &body[1..body.len() - 1]
    } else if seg.is_some() {
        body
    } else {
        bail!("not a memory operand: {}", s);
    };

    let mut m = Mem { size, seg, ..Default::default() };
    let inner = strip_rel(inner);
    let mut abs_acc: i64 = 0;
    let mut have_symbol = false;

    for (sign, term) in split_terms(inner) {
        // register * scale
        if let Some(star) = term.find('*') {
            let (rs, ss) = term.split_at(star);
            let scale: u8 = ss[1..].trim().parse().unwrap_or(1);
            let r = parse_reg(rs)
                .ok_or_else(|| anyhow::anyhow!("invalid index register '{}'", rs.trim()))?;
            m.index = Some(r);
            m.scale = scale;
            continue;
        }
        if let Some(r) = parse_reg(&term) {
            if term.eq_ignore_ascii_case("rip") {
                bail!("explicit rip-relative operands are not supported");
            }
            if m.base.is_none() {
                m.base = Some(r);
            } else if m.index.is_none() {
                m.index = Some(r);
                m.scale = 1;
            } else {
                bail!("too many registers in memory operand '{}'", s.trim());
            }
            continue;
        }
        if let Some(n) = parse_number(&term) {
            abs_acc += sign as i64 * n;
            continue;
        }
        if let Some(a) = resolve(&term) {
            abs_acc += sign as i64 * a as i64;
            have_symbol = true;
            continue;
        }
        bail!("cannot resolve '{}' in memory operand '{}'", term, s.trim());
    }

    if m.base.is_none() && m.index.is_none() {
        // Pure absolute address (symbol or literal) -> RIP-relative.
        m.abs = Some(abs_acc as u64);
        m.disp = abs_acc;
    } else {
        if have_symbol {
            // e.g. [rbx+someSymbol] - fold the symbol into the displacement.
        }
        m.disp = abs_acc;
    }
    Ok(m)
}

fn parse_operand(s: &str, resolve: Resolver) -> Result<Op> {
    let t = s.trim();
    if t.is_empty() {
        bail!("empty operand");
    }
    if let Some(r) = parse_reg(t) {
        return Ok(Op::R(r));
    }
    let (_, body) = strip_size_specifier(t);
    let (seg, body) = strip_segment(body);
    if body.trim_start().starts_with('[') || seg.is_some() {
        return Ok(Op::M(parse_mem(t, resolve)?));
    }
    // Immediate: a number, a symbol, or a sum of them. Whole-string first, so
    // hyphenated module names survive.
    if let Some(a) = resolve(t) {
        return Ok(Op::I(a as i64));
    }
    let terms = split_terms(t);
    let mut acc = 0i64;
    for (sign, term) in &terms {
        if let Some(n) = parse_number(term) {
            acc += *sign as i64 * n;
        } else if let Some(a) = resolve(term) {
            acc += *sign as i64 * a as i64;
        } else {
            bail!("cannot resolve operand '{}'", t);
        }
    }
    Ok(Op::I(acc))
}

/// Rejects a branch whose target lands inside the branch's own encoding.
///
/// A table may embed `EB 02` for a jump that skips two bytes; the equivalent rel32 is
/// five bytes long and swallows its own target, so the CPU jumps into the
/// middle of the `jmp` and decodes whatever byte it lands on. There is no
/// displacement that fixes this - the encoding is simply too long to express
/// the jump - so it must be an error rather than bytes that look plausible.
///
/// Silence here is the dangerous outcome: the emitted code is well-formed and
/// only corrupts control flow once a game thread runs it, which reads as the
/// table being broken. See the rel8 note in CLAUDE.md; relaxing branches to
/// rel8 during layout is the real fix and this guard is what proves it is
/// needed.
fn reject_self_overlapping_branch(mnem: &str, addr: u64, len: u64, target: u64) -> Result<()> {
    if target > addr && target < addr.wrapping_add(len) {
        bail!(
            "{} at {:#X} targets {:#X}, which is inside its own {}-byte encoding - \
             A short (rel8) branch would fit here and this assembler only emits \
             rel32, which cannot express a jump this short",
            mnem,
            addr,
            target,
            len
        );
    }
    Ok(())
}

/// Displacement of `target` from `next` (the end of the branch), or `None`
/// when a rel32 cannot express it.
///
/// Addresses wrap at 4GB in 32-bit mode, so there a rel32 always reaches and
/// the far-pointer fallback is never needed.
fn rel32(mode: Mode, target: u64, next: u64) -> Option<i32> {
    match mode {
        Mode::Bits32 => Some(target.wrapping_sub(next) as u32 as i32),
        Mode::Bits64 => {
            let d = target.wrapping_sub(next) as i64;
            (d >= i32::MIN as i64 && d <= i32::MAX as i64).then_some(d as i32)
        }
    }
}

/// Strips a branch-distance hint. Tables write `jmp short x`; the distance is
/// implied by the encoding chosen here, so the keyword carries no information.
fn strip_distance_hint(s: &str) -> &str {
    let t = s.trim_start();
    for kw in ["short ", "near ", "far ", "SHORT ", "NEAR ", "FAR "] {
        if let Some(rest) = t.strip_prefix(kw) {
            return rest.trim_start();
        }
    }
    t
}

/// Splits a source line into a mnemonic and its comma-separated operands.
fn split_line(line: &str) -> (String, Vec<String>) {
    let line = line.trim();
    let (mnem, rest) = match line.find(char::is_whitespace) {
        Some(i) => (&line[..i], line[i..].trim()),
        None => (line, ""),
    };
    let mut ops = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in rest.chars() {
        match ch {
            '[' | '(' => {
                depth += 1;
                cur.push(ch);
            }
            ']' | ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                ops.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        ops.push(cur.trim().to_string());
    }
    (mnem.to_lowercase(), ops)
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

fn op_size(op: &Op) -> Option<u8> {
    match op {
        Op::R(r) => Some(r.size()),
        Op::M(m) => m.size,
        Op::I(_) => None,
    }
}

fn imm_bytes(v: i64, size: u8) -> Vec<u8> {
    match size {
        1 => vec![v as i8 as u8],
        2 => (v as i16).to_le_bytes().to_vec(),
        4 => (v as i32).to_le_bytes().to_vec(),
        8 => v.to_le_bytes().to_vec(),
        _ => (v as i32).to_le_bytes().to_vec(),
    }
}

fn set_rm(insn: &mut Insn, op: &Op) -> Result<()> {
    match op {
        Op::R(r) => {
            insn.set_rm_reg(*r);
            Ok(())
        }
        Op::M(m) => insn.set_rm_mem(m),
        Op::I(_) => bail!("immediate cannot be an r/m operand"),
    }
}

const ALU: [(&str, u8); 8] = [
    ("add", 0),
    ("or", 1),
    ("adc", 2),
    ("sbb", 3),
    ("and", 4),
    ("sub", 5),
    ("xor", 6),
    ("cmp", 7),
];

const SHIFT: [(&str, u8); 6] = [
    ("rol", 0),
    ("ror", 1),
    ("shl", 4),
    ("sal", 4),
    ("shr", 5),
    ("sar", 7),
];

/// Condition-code suffixes for Jcc, in encoding order.
fn cond_code(s: &str) -> Option<u8> {
    Some(match s {
        "o" => 0,
        "no" => 1,
        "b" | "c" | "nae" => 2,
        "ae" | "nb" | "nc" => 3,
        "e" | "z" => 4,
        "ne" | "nz" => 5,
        "be" | "na" => 6,
        "a" | "nbe" => 7,
        "s" => 8,
        "ns" => 9,
        "p" | "pe" => 10,
        "np" | "po" => 11,
        "l" | "nge" => 12,
        "ge" | "nl" => 13,
        "le" | "ng" => 14,
        "g" | "nle" => 15,
        _ => return None,
    })
}

/// SSE instructions: (mnemonic, mandatory prefix, opcode bytes, direction-capable)
/// `store_op` is the opcode used when the destination is memory.
struct SseDef {
    prefix: Option<u8>,
    load: &'static [u8],
    store: Option<&'static [u8]>,
    /// Operand is a GPR on the r/m side (cvtsi2ss etc.)
    rm_gpr: bool,
    /// Destination is a GPR (cvtss2si etc.)
    dst_gpr: bool,
}

fn sse_def(m: &str) -> Option<SseDef> {
    let d = |prefix, load: &'static [u8], store: Option<&'static [u8]>| SseDef {
        prefix,
        load,
        store,
        rm_gpr: false,
        dst_gpr: false,
    };
    Some(match m {
        "movss" => d(Some(0xF3), &[0x0F, 0x10], Some(&[0x0F, 0x11])),
        "movsd" => d(Some(0xF2), &[0x0F, 0x10], Some(&[0x0F, 0x11])),
        "movups" => d(None, &[0x0F, 0x10], Some(&[0x0F, 0x11])),
        "movupd" => d(Some(0x66), &[0x0F, 0x10], Some(&[0x0F, 0x11])),
        "movaps" => d(None, &[0x0F, 0x28], Some(&[0x0F, 0x29])),
        "movapd" => d(Some(0x66), &[0x0F, 0x28], Some(&[0x0F, 0x29])),
        "movdqa" => d(Some(0x66), &[0x0F, 0x6F], Some(&[0x0F, 0x7F])),
        "movdqu" => d(Some(0xF3), &[0x0F, 0x6F], Some(&[0x0F, 0x7F])),
        "movlps" => d(None, &[0x0F, 0x12], Some(&[0x0F, 0x13])),
        "movhps" => d(None, &[0x0F, 0x16], Some(&[0x0F, 0x17])),

        "addss" => d(Some(0xF3), &[0x0F, 0x58], None),
        "subss" => d(Some(0xF3), &[0x0F, 0x5C], None),
        "mulss" => d(Some(0xF3), &[0x0F, 0x59], None),
        "divss" => d(Some(0xF3), &[0x0F, 0x5E], None),
        "minss" => d(Some(0xF3), &[0x0F, 0x5D], None),
        "maxss" => d(Some(0xF3), &[0x0F, 0x5F], None),
        "sqrtss" => d(Some(0xF3), &[0x0F, 0x51], None),

        "addsd" => d(Some(0xF2), &[0x0F, 0x58], None),
        "subsd" => d(Some(0xF2), &[0x0F, 0x5C], None),
        "mulsd" => d(Some(0xF2), &[0x0F, 0x59], None),
        "divsd" => d(Some(0xF2), &[0x0F, 0x5E], None),
        "minsd" => d(Some(0xF2), &[0x0F, 0x5D], None),
        "maxsd" => d(Some(0xF2), &[0x0F, 0x5F], None),
        "sqrtsd" => d(Some(0xF2), &[0x0F, 0x51], None),

        "addps" => d(None, &[0x0F, 0x58], None),
        "subps" => d(None, &[0x0F, 0x5C], None),
        "mulps" => d(None, &[0x0F, 0x59], None),
        "divps" => d(None, &[0x0F, 0x5E], None),
        "andps" => d(None, &[0x0F, 0x54], None),
        "andnps" => d(None, &[0x0F, 0x55], None),
        "orps" => d(None, &[0x0F, 0x56], None),
        "xorps" => d(None, &[0x0F, 0x57], None),
        "andpd" => d(Some(0x66), &[0x0F, 0x54], None),
        "xorpd" => d(Some(0x66), &[0x0F, 0x57], None),

        "addpd" => d(Some(0x66), &[0x0F, 0x58], None),
        "subpd" => d(Some(0x66), &[0x0F, 0x5C], None),
        "mulpd" => d(Some(0x66), &[0x0F, 0x59], None),
        "divpd" => d(Some(0x66), &[0x0F, 0x5E], None),

        "comiss" => d(None, &[0x0F, 0x2F], None),
        "ucomiss" => d(None, &[0x0F, 0x2E], None),
        "comisd" => d(Some(0x66), &[0x0F, 0x2F], None),
        "ucomisd" => d(Some(0x66), &[0x0F, 0x2E], None),

        "cvtdq2ps" => d(None, &[0x0F, 0x5B], None),
        "cvtdq2pd" => d(Some(0xF3), &[0x0F, 0xE6], None),
        "cvtpd2dq" => d(Some(0xF2), &[0x0F, 0xE6], None),
        "cvttpd2dq" => d(Some(0x66), &[0x0F, 0xE6], None),
        "cvtps2dq" => d(Some(0x66), &[0x0F, 0x5B], None),
        "cvttps2dq" => d(Some(0xF3), &[0x0F, 0x5B], None),
        "cvtss2sd" => d(Some(0xF3), &[0x0F, 0x5A], None),
        "cvtsd2ss" => d(Some(0xF2), &[0x0F, 0x5A], None),
        "cvtps2pd" => d(None, &[0x0F, 0x5A], None),
        "cvtpd2ps" => d(Some(0x66), &[0x0F, 0x5A], None),

        "pxor" => d(Some(0x66), &[0x0F, 0xEF], None),
        "pand" => d(Some(0x66), &[0x0F, 0xDB], None),
        "por" => d(Some(0x66), &[0x0F, 0xEB], None),
        "paddd" => d(Some(0x66), &[0x0F, 0xFE], None),
        "psubd" => d(Some(0x66), &[0x0F, 0xFA], None),
        "psubq" => d(Some(0x66), &[0x0F, 0xFB], None),
        "paddq" => d(Some(0x66), &[0x0F, 0xD4], None),
        "paddb" => d(Some(0x66), &[0x0F, 0xFC], None),
        "paddw" => d(Some(0x66), &[0x0F, 0xFD], None),
        "psubb" => d(Some(0x66), &[0x0F, 0xF8], None),
        "psubw" => d(Some(0x66), &[0x0F, 0xF9], None),
        "pmuludq" => d(Some(0x66), &[0x0F, 0xF4], None),
        "pandn" => d(Some(0x66), &[0x0F, 0xDF], None),
        "pcmpeqd" => d(Some(0x66), &[0x0F, 0x76], None),
        "pcmpeqb" => d(Some(0x66), &[0x0F, 0x74], None),
        "rcpss" => d(Some(0xF3), &[0x0F, 0x53], None),
        "rsqrtss" => d(Some(0xF3), &[0x0F, 0x52], None),
        "movnti" => d(None, &[0x0F, 0xC3], None),
        "movhlps" => d(None, &[0x0F, 0x12], None),
        "movlhps" => d(None, &[0x0F, 0x16], None),
        "unpcklps" => d(None, &[0x0F, 0x14], None),
        "unpckhps" => d(None, &[0x0F, 0x15], None),
        "punpckhwd" => d(Some(0x66), &[0x0F, 0x69], None),
        "punpcklwd" => d(Some(0x66), &[0x0F, 0x61], None),

        "cvtsi2ss" => SseDef {
            prefix: Some(0xF3),
            load: &[0x0F, 0x2A],
            store: None,
            rm_gpr: true,
            dst_gpr: false,
        },
        "cvtsi2sd" => SseDef {
            prefix: Some(0xF2),
            load: &[0x0F, 0x2A],
            store: None,
            rm_gpr: true,
            dst_gpr: false,
        },
        "cvtss2si" => SseDef {
            prefix: Some(0xF3),
            load: &[0x0F, 0x2D],
            store: None,
            rm_gpr: false,
            dst_gpr: true,
        },
        "cvttss2si" => SseDef {
            prefix: Some(0xF3),
            load: &[0x0F, 0x2C],
            store: None,
            rm_gpr: false,
            dst_gpr: true,
        },
        "cvtsd2si" => SseDef {
            prefix: Some(0xF2),
            load: &[0x0F, 0x2D],
            store: None,
            rm_gpr: false,
            dst_gpr: true,
        },
        "cvttsd2si" => SseDef {
            prefix: Some(0xF2),
            load: &[0x0F, 0x2C],
            store: None,
            rm_gpr: false,
            dst_gpr: true,
        },
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Assembles a single instruction at `addr`.
///
/// `resolve` maps symbol/label names to absolute addresses. During a layout pass
/// it may return `None` for not-yet-known labels; encoded lengths do not depend
/// on the resolved value, so sizes stay stable.
pub fn assemble(line: &str, addr: u64, resolve: Resolver) -> Result<Vec<u8>> {
    assemble_with(line, addr, resolve, &|_| None)
}

/// [`assemble`], plus a source of pointer slots for out-of-range branches.
pub fn assemble_with(
    line: &str,
    addr: u64,
    resolve: Resolver,
    far: FarSlots,
) -> Result<Vec<u8>> {
    assemble_mode(Mode::Bits64, line, addr, resolve, far)
}

/// [`assemble_with`] for a given target bitness. `far` is never consulted in
/// 32-bit mode.
pub fn assemble_mode(
    mode: Mode,
    line: &str,
    addr: u64,
    resolve: Resolver,
    far: FarSlots,
) -> Result<Vec<u8>> {
    let (mnem, ops) = split_line(line);
    if mnem.is_empty() {
        return Ok(Vec::new());
    }

    // Branch targets resolve to absolute addresses. An unknown name is an
    // error: silently falling back to `addr` assembled `call kernel32.Sleep`
    // into a call-to-itself. The caller's resolver decides leniency - during
    // the layout pass it returns a placeholder so lengths stay correct.
    let branch_target = |s: &str| -> Result<u64> {
        let t = strip_distance_hint(s).trim();
        if let Some(v) = parse_number(t) {
            return Ok(v as u64);
        }
        // Try the whole string first: module names legitimately contain '-'
        // (`Palworld-Win64-Shipping.exe+1234`), which term-splitting would
        // otherwise read as subtraction.
        if let Some(a) = resolve(t) {
            return Ok(a);
        }
        let mut acc = 0i64;
        for (sign, term) in split_terms(t) {
            if let Some(n) = parse_number(&term) {
                acc += sign as i64 * n;
            } else if let Some(a) = resolve(&term) {
                acc += sign as i64 * a as i64;
            } else {
                bail!("cannot resolve branch target '{}'", term);
            }
        }
        Ok(acc as u64)
    };

    let mut insn = Insn { mode, ..Default::default() };
    let only64 = |what: &str| -> Result<()> {
        if mode == Mode::Bits32 {
            bail!("{} does not exist in 32-bit mode", what);
        }
        Ok(())
    };

    match mnem.as_str() {
        // ---- zero-operand -------------------------------------------------
        "nop" if ops.is_empty() => return Ok(vec![0x90]),
        // `nop dword ptr [rax]` and friends: the multi-byte padding form.
        "nop" => {
            let op = parse_operand(&ops[0], resolve)?;
            let size = op_size(&op).unwrap_or(4);
            insn.apply_operand_size(size);
            insn.opcode = vec![0x0F, 0x1F];
            insn.set_reg_raw(0);
            set_rm(&mut insn, &op)?;
            return insn.emit(addr);
        }
        "ret" | "retn" => {
            if ops.is_empty() {
                return Ok(vec![0xC3]);
            }
            let n = parse_number(&ops[0]).unwrap_or(0);
            let mut v = vec![0xC2];
            v.extend_from_slice(&(n as u16).to_le_bytes());
            return Ok(v);
        }
        "int3" => return Ok(vec![0xCC]),
        "pushfq" | "pushf" | "pushfd" => return Ok(vec![0x9C]),
        "popfq" | "popf" | "popfd" => return Ok(vec![0x9D]),
        // 32-bit only, but common in older tables.
        "pushad" | "pusha" => return Ok(vec![0x60]),
        "popad" | "popa" => return Ok(vec![0x61]),
        "cdq" => return Ok(vec![0x99]),
        "cqo" => {
            only64("cqo")?;
            return Ok(vec![0x48, 0x99]);
        }
        "cdqe" => {
            only64("cdqe")?;
            return Ok(vec![0x48, 0x98]);
        }
        "leave" => return Ok(vec![0xC9]),
        "enter" => {
            let frame = ops.first().and_then(|s| parse_number(s)).unwrap_or(0);
            let level = ops.get(1).and_then(|s| parse_number(s)).unwrap_or(0);
            let mut v = vec![0xC8];
            v.extend_from_slice(&(frame as u16).to_le_bytes());
            v.push(level as u8);
            return Ok(v);
        }
        "int" => {
            let n = ops.first().and_then(|s| parse_number(s)).unwrap_or(3);
            return Ok(if n == 3 { vec![0xCC] } else { vec![0xCD, n as u8] });
        }
        "hlt" => return Ok(vec![0xF4]),
        "stc" => return Ok(vec![0xF9]),
        "clc" => return Ok(vec![0xF8]),
        "sti" => return Ok(vec![0xFB]),
        "cli" => return Ok(vec![0xFA]),
        "pause" => return Ok(vec![0xF3, 0x90]),
        "cld" => return Ok(vec![0xFC]),
        // Legacy BCD helpers, common in 32-bit era tables.
        "aaa" => return Ok(vec![0x37]),
        "aas" => return Ok(vec![0x3F]),
        "daa" => return Ok(vec![0x27]),
        "das" => return Ok(vec![0x2F]),
        "aad" => return Ok(vec![0xD5, 0x0A]),
        "aam" => return Ok(vec![0xD4, 0x0A]),
        "into" => return Ok(vec![0xCE]),
        "salc" => return Ok(vec![0xD6]),
        "xlat" | "xlatb" => return Ok(vec![0xD7]),
        "std" => return Ok(vec![0xFD]),
        "cmc" => return Ok(vec![0xF5]),
        "sahf" => return Ok(vec![0x9E]),
        "lahf" => return Ok(vec![0x9F]),
        "cpuid" => return Ok(vec![0x0F, 0xA2]),
        "rdtsc" => return Ok(vec![0x0F, 0x31]),
        "ud2" => return Ok(vec![0x0F, 0x0B]),
        "movsb" => return Ok(vec![0xA4]),
        "movsw" => return Ok(vec![0x66, 0xA5]),
        "movsq" => {
            only64("movsq")?;
            return Ok(vec![0x48, 0xA5]);
        }
        "stosb" => return Ok(vec![0xAA]),
        "stosw" => return Ok(vec![0x66, 0xAB]),
        "stosq" => {
            only64("stosq")?;
            return Ok(vec![0x48, 0xAB]);
        }
        "lodsb" => return Ok(vec![0xAC]),
        "scasb" => return Ok(vec![0xAE]),
        "cmpsb" => return Ok(vec![0xA6]),
        // `movsd`/`stosd`/`lodsd` collide with SSE mnemonics; only the
        // operand-less form is the string instruction.
        "stosd" if ops.is_empty() => return Ok(vec![0xAB]),
        "lodsd" if ops.is_empty() => return Ok(vec![0xAD]),
        "scasd" if ops.is_empty() => return Ok(vec![0xAF]),
        "cmpsd" if ops.is_empty() => return Ok(vec![0xA7]),
        "movsd" if ops.is_empty() => return Ok(vec![0xA5]),
        _ => {}
    }

    // ---- rep / lock prefixes ---------------------------------------------
    for (name, prefix) in [
        ("rep", 0xF3u8),
        ("repe", 0xF3),
        ("repz", 0xF3),
        ("repne", 0xF2),
        ("repnz", 0xF2),
        ("lock", 0xF0),
    ] {
        if mnem == name {
            let rest = line.trim()[name.len()..].trim();
            if rest.is_empty() {
                return Ok(vec![prefix]);
            }
            let mut v = vec![prefix];
            v.extend_from_slice(&assemble_mode(mode, rest, addr.wrapping_add(1), resolve, far)?);
            return Ok(v);
        }
    }

    match mnem.as_str() {
        // ---- loop -------------------------------------------------------
        "loop" | "loope" | "loopne" | "loopz" | "loopnz" | "jrcxz" | "jecxz" => {
            let t = ops.first().ok_or_else(|| anyhow::anyhow!("{} needs a target", mnem))?;
            let target = branch_target(t)?;
            let rel = match mode {
                Mode::Bits32 => target.wrapping_sub(addr.wrapping_add(2)) as u32 as i32 as i64,
                Mode::Bits64 => target.wrapping_sub(addr.wrapping_add(2)) as i64,
            };
            if !(-128..=127).contains(&rel) {
                bail!("{} target is out of the ±128 byte range of a short branch", mnem);
            }
            let op = match mnem.as_str() {
                "loop" => 0xE2,
                "loope" | "loopz" => 0xE1,
                "loopne" | "loopnz" => 0xE0,
                _ => 0xE3,
            };
            return Ok(vec![op, rel as i8 as u8]);
        }

        // ---- branches -----------------------------------------------------
        "jmp" | "call" => {
            let t = ops.first().ok_or_else(|| anyhow::anyhow!("{} needs a target", mnem))?;
            // Indirect forms: `jmp rax`, `jmp [rax]`, `call qword ptr [rax+40]`.
            // Any register or memory operand is indirect; a direct near branch
            // target parses as an immediate.
            if let Ok(op) = parse_operand(t, resolve) {
                let indirect = matches!(op, Op::R(_) | Op::M(_));
                if indirect {
                    insn.opcode = vec![0xFF];
                    insn.set_reg_raw(if mnem == "jmp" { 4 } else { 2 });
                    set_rm(&mut insn, &op)?;
                    return insn.emit(addr);
                }
            }
            let target = branch_target(t)?;
            let Some(rel) = rel32(mode, target, addr.wrapping_add(5)) else {
                // Too far for rel32: go indirect through a pointer slot,
                // `call/jmp qword ptr [rip+disp32]` (always 6 bytes, so the
                // layout pass measures the same length).
                let Some(slot) = far(target) else {
                    bail!("{} target {:#X} is out of ±2GB range from {:#X}", mnem, target, addr);
                };
                let disp = slot.wrapping_sub(addr.wrapping_add(6)) as i64;
                if disp < i32::MIN as i64 || disp > i32::MAX as i64 {
                    bail!("{}: pointer slot {:#X} is itself out of range", mnem, slot);
                }
                let mut v = vec![0xFF, if mnem == "jmp" { 0x25 } else { 0x15 }];
                v.extend_from_slice(&(disp as i32).to_le_bytes());
                return Ok(v);
            };
            reject_self_overlapping_branch(&mnem, addr, 5, target)?;
            let mut v = vec![if mnem == "jmp" { 0xE9 } else { 0xE8 }];
            v.extend_from_slice(&rel.to_le_bytes());
            return Ok(v);
        }

        // ---- push / pop ---------------------------------------------------
        "push" => {
            let t = ops.first().ok_or_else(|| anyhow::anyhow!("push needs an operand"))?;
            match parse_operand(t, resolve)? {
                // 0x50+rd covers `push rax` (64-bit mode) and `push eax`
                // (32-bit tables); 16-bit takes an operand-size prefix.
                Op::R(r) if matches!(r.kind, RegKind::G64 | RegKind::G32 | RegKind::G16) => {
                    if r.kind == RegKind::G64 || r.num >= 8 {
                        only64("push of a 64-bit or extended register")?;
                    }
                    let mut v = Vec::new();
                    if r.kind == RegKind::G16 {
                        v.push(0x66);
                    }
                    if r.num >= 8 {
                        v.push(0x41);
                    }
                    v.push(0x50 + (r.num & 7));
                    return Ok(v);
                }
                Op::I(n) => {
                    if (-128..=127).contains(&n) {
                        return Ok(vec![0x6A, n as i8 as u8]);
                    }
                    let mut v = vec![0x68];
                    v.extend_from_slice(&(n as i32).to_le_bytes());
                    return Ok(v);
                }
                op @ Op::M(_) => {
                    insn.opcode = vec![0xFF];
                    insn.set_reg_raw(6);
                    set_rm(&mut insn, &op)?;
                    return insn.emit(addr);
                }
                _ => bail!("unsupported push operand '{}'", t),
            }
        }
        "pop" => {
            let t = ops.first().ok_or_else(|| anyhow::anyhow!("pop needs an operand"))?;
            match parse_operand(t, resolve)? {
                Op::R(r) if matches!(r.kind, RegKind::G64 | RegKind::G32 | RegKind::G16) => {
                    if r.kind == RegKind::G64 || r.num >= 8 {
                        only64("pop of a 64-bit or extended register")?;
                    }
                    let mut v = Vec::new();
                    if r.kind == RegKind::G16 {
                        v.push(0x66);
                    }
                    if r.num >= 8 {
                        v.push(0x41);
                    }
                    v.push(0x58 + (r.num & 7));
                    return Ok(v);
                }
                op @ Op::M(_) => {
                    insn.opcode = vec![0x8F];
                    insn.set_reg_raw(0);
                    set_rm(&mut insn, &op)?;
                    return insn.emit(addr);
                }
                _ => bail!("unsupported pop operand '{}'", t),
            }
        }
        _ => {}
    }

    // ---- Jcc --------------------------------------------------------------
    if let Some(rest) = mnem.strip_prefix('j') {
        if let Some(cc) = cond_code(rest) {
            let t = ops.first().ok_or_else(|| anyhow::anyhow!("{} needs a target", mnem))?;
            let target = branch_target(t)?;
            let Some(rel) = rel32(mode, target, addr.wrapping_add(6)) else {
                bail!("{} target {:#X} is out of ±2GB range", mnem, target);
            };
            reject_self_overlapping_branch(&mnem, addr, 6, target)?;
            let mut v = vec![0x0F, 0x80 + cc];
            v.extend_from_slice(&rel.to_le_bytes());
            return Ok(v);
        }
    }

    // ---- setcc ------------------------------------------------------------
    if let Some(rest) = mnem.strip_prefix("set") {
        if let Some(cc) = cond_code(rest) {
            let op = parse_operand(&ops[0], resolve)?;
            insn.opcode = vec![0x0F, 0x90 + cc];
            insn.set_reg_raw(0);
            set_rm(&mut insn, &op)?;
            return insn.emit(addr);
        }
    }

    // ---- cmovcc -----------------------------------------------------------
    if let Some(rest) = mnem.strip_prefix("cmov") {
        if let Some(cc) = cond_code(rest) {
            let dst = parse_operand(&ops[0], resolve)?;
            let src = parse_operand(&ops[1], resolve)?;
            let Op::R(d) = dst else { bail!("cmov destination must be a register") };
            insn.apply_operand_size(d.size());
            insn.opcode = vec![0x0F, 0x40 + cc];
            insn.set_reg(d);
            set_rm(&mut insn, &src)?;
            return insn.emit(addr);
        }
    }

    // ---- SSE with an imm8 selector ---------------------------------------
    if let Some((prefix, opcode)) = match mnem.as_str() {
        "shufps" => Some((None, [0x0F, 0xC6])),
        "shufpd" => Some((Some(0x66), [0x0F, 0xC6])),
        "pshufd" => Some((Some(0x66), [0x0F, 0x70])),
        "pshuflw" => Some((Some(0xF2), [0x0F, 0x70])),
        "pshufhw" => Some((Some(0xF3), [0x0F, 0x70])),
        "cmpps" => Some((None, [0x0F, 0xC2])),
        "cmpss" => Some((Some(0xF3), [0x0F, 0xC2])),
        "palignr" => None, // 3-byte opcode, not needed here
        _ => None,
    } {
        if ops.len() != 3 {
            bail!("{} expects 3 operands", mnem);
        }
        let Op::R(d) = parse_operand(&ops[0], resolve)? else {
            bail!("{}: destination must be a register", mnem)
        };
        let src = parse_operand(&ops[1], resolve)?;
        let Op::I(sel) = parse_operand(&ops[2], resolve)? else {
            bail!("{}: third operand must be an immediate", mnem)
        };
        if let Some(p) = prefix {
            insn.prefixes.push(p);
        }
        insn.opcode = opcode.to_vec();
        insn.set_reg(d);
        set_rm(&mut insn, &src)?;
        insn.imm = vec![sel as u8];
        return insn.emit(addr);
    }

    // ---- x87 compares that set EFLAGS ------------------------------------
    if let Some((op, base)) = match mnem.as_str() {
        "fcomi" => Some((0xDBu8, 0xF0u8)),
        "fcomip" => Some((0xDF, 0xF0)),
        "fucomi" => Some((0xDB, 0xE8)),
        "fucomip" => Some((0xDF, 0xE8)),
        _ => None,
    } {
        let i = ops.first().and_then(|s| parse_st(s)).unwrap_or(1);
        return Ok(vec![op, base + i]);
    }

    // ---- bit test / exchange ---------------------------------------------
    if let Some(ext) = match mnem.as_str() {
        "bt" => Some(4u8),
        "bts" => Some(5),
        "btr" => Some(6),
        "btc" => Some(7),
        _ => None,
    } {
        let dst = parse_operand(&ops[0], resolve)?;
        let src = parse_operand(&ops[1], resolve)?;
        let sz = op_size(&dst).unwrap_or(4);
        insn.apply_operand_size(sz);
        match src {
            Op::I(n) => {
                insn.opcode = vec![0x0F, 0xBA];
                insn.set_reg_raw(ext);
                set_rm(&mut insn, &dst)?;
                insn.imm = vec![n as u8];
            }
            Op::R(r) => {
                insn.opcode = vec![0x0F, 0xA3 + (ext - 4) * 8];
                insn.set_reg(r);
                set_rm(&mut insn, &dst)?;
            }
            _ => bail!("{}: unsupported operands", mnem),
        }
        return insn.emit(addr);
    }
    if mnem == "xadd" || mnem == "cmpxchg" {
        let dst = parse_operand(&ops[0], resolve)?;
        let Op::R(s) = parse_operand(&ops[1], resolve)? else {
            bail!("{}: source must be a register", mnem)
        };
        insn.apply_operand_size(s.size());
        let base = if mnem == "xadd" { 0xC0 } else { 0xB0 };
        insn.opcode = vec![0x0F, base + if s.size() == 1 { 0 } else { 1 }];
        insn.set_reg(s);
        set_rm(&mut insn, &dst)?;
        return insn.emit(addr);
    }
    if mnem == "bswap" {
        let Op::R(r) = parse_operand(&ops[0], resolve)? else {
            bail!("bswap needs a register")
        };
        insn.apply_operand_size(r.size());
        if r.num >= 8 {
            insn.rex_b = true;
        }
        insn.opcode = vec![0x0F, 0xC8 + (r.num & 7)];
        return insn.emit(addr);
    }

    // ---- AVX forms that take an imm8 selector ----------------------------
    if let Some(base) = mnem.strip_prefix('v') {
        if let Some((prefix, opcode)) = match base {
            "shufps" => Some((None, [0x0F, 0xC6])),
            "shufpd" => Some((Some(0x66u8), [0x0F, 0xC6])),
            "pshufd" => Some((Some(0x66), [0x0F, 0x70])),
            "cmpps" => Some((None, [0x0F, 0xC2])),
            "cmpss" => Some((Some(0xF3), [0x0F, 0xC2])),
            _ => None,
        } {
            if ops.len() < 3 {
                bail!("{} expects 3 or 4 operands", mnem);
            }
            let pp = match prefix {
                None => 0u8,
                Some(0x66) => 1,
                Some(0xF3) => 2,
                Some(0xF2) => 3,
                Some(p) => bail!("{}: unexpected prefix {:#X}", mnem, p),
            };
            let Op::R(d) = parse_operand(&ops[0], resolve)? else {
                bail!("{}: destination must be a register", mnem)
            };
            // 4-operand form is dst, src1, src2, imm8; 3-operand omits src1.
            let (vvvv, rm_idx) = if ops.len() >= 4 {
                match parse_operand(&ops[1], resolve)? {
                    Op::R(r) => (r.num, 2),
                    _ => bail!("{}: second operand must be a register", mnem),
                }
                } else {
                (0, 1)
            };
            let rm = parse_operand(&ops[rm_idx], resolve)?;
            let Op::I(sel) = parse_operand(&ops[ops.len() - 1], resolve)? else {
                bail!("{}: last operand must be an immediate", mnem)
            };
            insn.vex = Some(Vex { pp, mmmmm: 1, w: false, l: false, vvvv });
            insn.opcode = opcode.to_vec();
            insn.set_reg(d);
            set_rm(&mut insn, &rm)?;
            insn.imm = vec![sel as u8];
            return insn.emit(addr);
        }
    }

    // ---- AVX (VEX-encoded SSE) -------------------------------------------
    // `vmovss`, `vxorps`, `vcvtsi2ss` etc. reuse the SSE opcode table with a
    // VEX prefix instead of the legacy prefix + REX.
    if let Some(base) = mnem.strip_prefix('v') {
        if let Some(def) = sse_def(base) {
            if ops.len() < 2 || ops.len() > 3 {
                bail!("{} expects 2 or 3 operands", mnem);
            }
            let pp = match def.prefix {
                None => 0u8,
                Some(0x66) => 1,
                Some(0xF3) => 2,
                Some(0xF2) => 3,
                Some(p) => bail!("{}: unexpected prefix {:#X}", mnem, p),
            };

            let dst = parse_operand(&ops[0], resolve)?;
            // 3-operand form: dst, vvvv, rm. 2-operand form has no vvvv.
            let (vvvv_op, rm_op) = if ops.len() == 3 {
                (Some(parse_operand(&ops[1], resolve)?), parse_operand(&ops[2], resolve)?)
            } else {
                (None, parse_operand(&ops[1], resolve)?)
            };

            let vvvv = match vvvv_op {
                Some(Op::R(r)) => r.num,
                Some(_) => bail!("{}: second operand must be a register", mnem),
                None => 0,
            };

            let w = def.rm_gpr
                && match &rm_op {
                    Op::R(r) => r.size() == 8,
                    Op::M(m) => m.size == Some(8),
                    _ => false,
                };

            insn.vex = Some(Vex { pp, mmmmm: 1, w, l: false, vvvv });

            // Store form: memory destination uses the alternate opcode.
            if let (Op::M(_), Some(store)) = (&dst, def.store) {
                let Op::R(s) = rm_op else {
                    bail!("{}: source must be a register", mnem)
                };
                insn.opcode = store.to_vec();
                insn.set_reg(s);
                set_rm(&mut insn, &dst)?;
                return insn.emit(addr);
            }

            let Op::R(d) = dst else {
                bail!("{}: destination must be a register", mnem)
            };
            insn.opcode = def.load.to_vec();
            insn.set_reg(d);
            set_rm(&mut insn, &rm_op)?;
            return insn.emit(addr);
        }
    }

    // ---- SSE --------------------------------------------------------------
    if let Some(def) = sse_def(&mnem) {
        if ops.len() != 2 {
            bail!("{} expects 2 operands", mnem);
        }
        let dst = parse_operand(&ops[0], resolve)?;
        let src = parse_operand(&ops[1], resolve)?;

        if let Some(p) = def.prefix {
            insn.prefixes.push(p);
        }

        // Store form: destination is memory and the instruction has one.
        if let (Op::M(_), Some(store)) = (&dst, def.store) {
            let Op::R(s) = src else { bail!("{}: source must be a register", mnem) };
            insn.opcode = store.to_vec();
            insn.set_reg(s);
            set_rm(&mut insn, &dst)?;
            return insn.emit(addr);
        }

        let Op::R(d) = dst else {
            bail!("{}: destination must be a register", mnem)
        };
        if def.dst_gpr && d.size() == 8 {
            insn.rex_w = true;
        }
        if def.rm_gpr {
            if let Op::R(s) = src {
                if s.size() == 8 {
                    insn.rex_w = true;
                }
            } else if let Op::M(ref m) = src {
                if m.size == Some(8) {
                    insn.rex_w = true;
                }
            }
        }
        insn.opcode = def.load.to_vec();
        insn.set_reg(d);
        set_rm(&mut insn, &src)?;
        return insn.emit(addr);
    }

    // ---- movd / movq ------------------------------------------------------
    if mnem == "movd" || mnem == "movq" {
        let dst = parse_operand(&ops[0], resolve)?;
        let src = parse_operand(&ops[1], resolve)?;
        let wide = mnem == "movq";
        match (&dst, &src) {
            (Op::R(d), _) if d.kind == RegKind::Xmm => {
                // movq xmm, xmm/m64 has a dedicated F3 0F 7E form.
                if wide && matches!(src, Op::M(_)) {
                    insn.prefixes.push(0xF3);
                    insn.opcode = vec![0x0F, 0x7E];
                } else {
                    insn.prefixes.push(0x66);
                    insn.opcode = vec![0x0F, 0x6E];
                    insn.rex_w = wide;
                }
                insn.set_reg(*d);
                set_rm(&mut insn, &src)?;
                return insn.emit(addr);
            }
            (_, Op::R(s)) if s.kind == RegKind::Xmm => {
                if wide && matches!(dst, Op::M(_)) {
                    insn.prefixes.push(0x66);
                    insn.opcode = vec![0x0F, 0xD6];
                } else {
                    insn.prefixes.push(0x66);
                    insn.opcode = vec![0x0F, 0x7E];
                    insn.rex_w = wide;
                }
                insn.set_reg(*s);
                set_rm(&mut insn, &dst)?;
                return insn.emit(addr);
            }
            _ => bail!("{} requires an xmm operand", mnem),
        }
    }

    // ---- mov --------------------------------------------------------------
    if mnem == "mov" {
        if ops.len() != 2 {
            bail!("mov expects 2 operands");
        }
        let dst = parse_operand(&ops[0], resolve)?;
        let src = parse_operand(&ops[1], resolve)?;

        match (&dst, &src) {
            (Op::R(d), Op::R(s)) => {
                if d.size() != s.size() {
                    bail!("mov operand size mismatch: {} / {}", ops[0], ops[1]);
                }
                insn.apply_operand_size(d.size());
                insn.opcode = vec![if d.size() == 1 { 0x88 } else { 0x89 }];
                insn.set_reg(*s);
                insn.set_rm_reg(*d);
                return insn.emit(addr);
            }
            (Op::R(d), Op::M(_)) => {
                insn.apply_operand_size(d.size());
                insn.opcode = vec![if d.size() == 1 { 0x8A } else { 0x8B }];
                insn.set_reg(*d);
                set_rm(&mut insn, &src)?;
                return insn.emit(addr);
            }
            (Op::M(_), Op::R(s)) => {
                insn.apply_operand_size(s.size());
                insn.opcode = vec![if s.size() == 1 { 0x88 } else { 0x89 }];
                insn.set_reg(*s);
                set_rm(&mut insn, &dst)?;
                return insn.emit(addr);
            }
            (Op::R(d), Op::I(n)) => {
                let sz = d.size();
                if sz == 8 {
                    only64("a 64-bit register")?;
                    // Prefer the compact 32-bit sign-extended form when possible.
                    if *n >= i32::MIN as i64 && *n <= i32::MAX as i64 {
                        insn.rex_w = true;
                        insn.opcode = vec![0xC7];
                        insn.set_reg_raw(0);
                        insn.set_rm_reg(*d);
                        insn.imm = imm_bytes(*n, 4);
                        return insn.emit(addr);
                    }
                    let mut v = vec![0x48 | if d.num >= 8 { 1 } else { 0 }];
                    v.push(0xB8 + (d.num & 7));
                    v.extend_from_slice(&n.to_le_bytes());
                    return Ok(v);
                }
                // Short B0+rb / B8+rd form, the conventional encoding.
                insn.apply_operand_size(sz);
                if d.num >= 8 {
                    insn.rex_b = true;
                }
                if d.kind == RegKind::G8Rex {
                    insn.need_rex = true;
                }
                if d.kind == RegKind::G8High {
                    insn.forbid_rex = true;
                }
                insn.opcode = vec![if sz == 1 { 0xB0 } else { 0xB8 } + (d.num & 7)];
                insn.imm = imm_bytes(*n, if sz == 1 { 1 } else if sz == 2 { 2 } else { 4 });
                return insn.emit(addr);
            }
            (Op::M(m), Op::I(n)) => {
                // An unsized memory destination defaults to dword.
                let sz = m.size.unwrap_or(4);
                insn.apply_operand_size(sz);
                insn.opcode = vec![if sz == 1 { 0xC6 } else { 0xC7 }];
                insn.set_reg_raw(0);
                set_rm(&mut insn, &dst)?;
                insn.imm = imm_bytes(*n, if sz == 1 { 1 } else if sz == 2 { 2 } else { 4 });
                return insn.emit(addr);
            }
            _ => bail!("unsupported mov form: {}", line.trim()),
        }
    }

    // ---- lea --------------------------------------------------------------
    if mnem == "lea" {
        let Op::R(d) = parse_operand(&ops[0], resolve)? else {
            bail!("lea destination must be a register")
        };
        let src = parse_operand(&ops[1], resolve)?;
        if !matches!(src, Op::M(_)) {
            bail!("lea source must be a memory operand");
        }
        insn.apply_operand_size(d.size());
        insn.opcode = vec![0x8D];
        insn.set_reg(d);
        set_rm(&mut insn, &src)?;
        return insn.emit(addr);
    }

    // ---- movzx / movsx / movsxd ------------------------------------------
    if mnem == "movzx" || mnem == "movsx" || mnem == "movsxd" {
        let Op::R(d) = parse_operand(&ops[0], resolve)? else {
            bail!("{} destination must be a register", mnem)
        };
        let src = parse_operand(&ops[1], resolve)?;
        let src_sz = op_size(&src).unwrap_or(1);
        if mnem == "movsxd" {
            only64("movsxd")?;
        }
        insn.apply_operand_size(d.size());
        insn.opcode = if mnem == "movsxd" {
            vec![0x63]
        } else if mnem == "movzx" {
            vec![0x0F, if src_sz == 2 { 0xB7 } else { 0xB6 }]
        } else {
            vec![0x0F, if src_sz == 2 { 0xBF } else { 0xBE }]
        };
        insn.set_reg(d);
        set_rm(&mut insn, &src)?;
        return insn.emit(addr);
    }

    // ---- xchg -------------------------------------------------------------
    if mnem == "xchg" {
        let a = parse_operand(&ops[0], resolve)?;
        let b = parse_operand(&ops[1], resolve)?;
        let (r, rm) = match (&a, &b) {
            (Op::R(r), _) => (*r, b.clone()),
            (_, Op::R(r)) => (*r, a.clone()),
            _ => bail!("xchg needs at least one register"),
        };
        insn.apply_operand_size(r.size());
        insn.opcode = vec![if r.size() == 1 { 0x86 } else { 0x87 }];
        insn.set_reg(r);
        set_rm(&mut insn, &rm)?;
        return insn.emit(addr);
    }

    // ---- ALU group --------------------------------------------------------
    if let Some((_, idx)) = ALU.iter().find(|(n, _)| *n == mnem) {
        if ops.len() != 2 {
            bail!("{} expects 2 operands", mnem);
        }
        let dst = parse_operand(&ops[0], resolve)?;
        let src = parse_operand(&ops[1], resolve)?;
        let idx = *idx;

        match (&dst, &src) {
            (_, Op::I(n)) => {
                let sz = op_size(&dst).unwrap_or(4);
                insn.apply_operand_size(sz);
                let fits_i8 = *n >= -128 && *n <= 127;
                if sz == 1 {
                    insn.opcode = vec![0x80];
                    insn.imm = imm_bytes(*n, 1);
                } else if fits_i8 {
                    insn.opcode = vec![0x83];
                    insn.imm = imm_bytes(*n, 1);
                } else {
                    insn.opcode = vec![0x81];
                    insn.imm = imm_bytes(*n, if sz == 2 { 2 } else { 4 });
                }
                insn.set_reg_raw(idx);
                set_rm(&mut insn, &dst)?;
                return insn.emit(addr);
            }
            (Op::R(d), Op::R(s)) => {
                if d.size() != s.size() {
                    bail!("{} operand size mismatch", mnem);
                }
                insn.apply_operand_size(d.size());
                insn.opcode = vec![idx * 8 + if d.size() == 1 { 0x00 } else { 0x01 }];
                insn.set_reg(*s);
                insn.set_rm_reg(*d);
                return insn.emit(addr);
            }
            (Op::R(d), Op::M(_)) => {
                insn.apply_operand_size(d.size());
                insn.opcode = vec![idx * 8 + if d.size() == 1 { 0x02 } else { 0x03 }];
                insn.set_reg(*d);
                set_rm(&mut insn, &src)?;
                return insn.emit(addr);
            }
            (Op::M(_), Op::R(s)) => {
                insn.apply_operand_size(s.size());
                insn.opcode = vec![idx * 8 + if s.size() == 1 { 0x00 } else { 0x01 }];
                insn.set_reg(*s);
                set_rm(&mut insn, &dst)?;
                return insn.emit(addr);
            }
            _ => bail!("unsupported {} form: {}", mnem, line.trim()),
        }
    }

    // ---- test -------------------------------------------------------------
    if mnem == "test" {
        let dst = parse_operand(&ops[0], resolve)?;
        let src = parse_operand(&ops[1], resolve)?;
        match (&dst, &src) {
            (_, Op::I(n)) => {
                let sz = op_size(&dst).unwrap_or(4);
                insn.apply_operand_size(sz);
                insn.opcode = vec![if sz == 1 { 0xF6 } else { 0xF7 }];
                insn.set_reg_raw(0);
                set_rm(&mut insn, &dst)?;
                insn.imm = imm_bytes(*n, if sz == 1 { 1 } else if sz == 2 { 2 } else { 4 });
                return insn.emit(addr);
            }
            (_, Op::R(s)) => {
                insn.apply_operand_size(s.size());
                insn.opcode = vec![if s.size() == 1 { 0x84 } else { 0x85 }];
                insn.set_reg(*s);
                set_rm(&mut insn, &dst)?;
                return insn.emit(addr);
            }
            _ => bail!("unsupported test form"),
        }
    }

    // ---- unary group (F6/F7 and FE/FF) -----------------------------------
    for (name, ext) in [("not", 2u8), ("neg", 3), ("mul", 4), ("div", 6), ("idiv", 7)] {
        if mnem == name {
            let op = parse_operand(&ops[0], resolve)?;
            let sz = op_size(&op).unwrap_or(4);
            insn.apply_operand_size(sz);
            insn.opcode = vec![if sz == 1 { 0xF6 } else { 0xF7 }];
            insn.set_reg_raw(ext);
            set_rm(&mut insn, &op)?;
            return insn.emit(addr);
        }
    }
    if mnem == "inc" || mnem == "dec" {
        let op = parse_operand(&ops[0], resolve)?;
        let sz = op_size(&op).unwrap_or(4);
        insn.apply_operand_size(sz);
        insn.opcode = vec![if sz == 1 { 0xFE } else { 0xFF }];
        insn.set_reg_raw(if mnem == "inc" { 0 } else { 1 });
        set_rm(&mut insn, &op)?;
        return insn.emit(addr);
    }

    // ---- imul -------------------------------------------------------------
    if mnem == "imul" {
        match ops.len() {
            1 => {
                let op = parse_operand(&ops[0], resolve)?;
                let sz = op_size(&op).unwrap_or(4);
                insn.apply_operand_size(sz);
                insn.opcode = vec![if sz == 1 { 0xF6 } else { 0xF7 }];
                insn.set_reg_raw(5);
                set_rm(&mut insn, &op)?;
                return insn.emit(addr);
            }
            2 => {
                let Op::R(d) = parse_operand(&ops[0], resolve)? else {
                    bail!("imul destination must be a register")
                };
                let src = parse_operand(&ops[1], resolve)?;
                insn.apply_operand_size(d.size());
                insn.opcode = vec![0x0F, 0xAF];
                insn.set_reg(d);
                set_rm(&mut insn, &src)?;
                return insn.emit(addr);
            }
            3 => {
                let Op::R(d) = parse_operand(&ops[0], resolve)? else {
                    bail!("imul destination must be a register")
                };
                let src = parse_operand(&ops[1], resolve)?;
                let Op::I(n) = parse_operand(&ops[2], resolve)? else {
                    bail!("imul third operand must be an immediate")
                };
                insn.apply_operand_size(d.size());
                let fits = (-128..=127).contains(&n);
                insn.opcode = vec![if fits { 0x6B } else { 0x69 }];
                insn.set_reg(d);
                set_rm(&mut insn, &src)?;
                insn.imm = imm_bytes(n, if fits { 1 } else { 4 });
                return insn.emit(addr);
            }
            _ => bail!("imul expects 1-3 operands"),
        }
    }

    // ---- shifts -----------------------------------------------------------
    if let Some((_, ext)) = SHIFT.iter().find(|(n, _)| *n == mnem) {
        let dst = parse_operand(&ops[0], resolve)?;
        let sz = op_size(&dst).unwrap_or(4);
        insn.apply_operand_size(sz);
        let by_cl = ops.len() > 1 && ops[1].trim().eq_ignore_ascii_case("cl");
        if by_cl {
            insn.opcode = vec![if sz == 1 { 0xD2 } else { 0xD3 }];
        } else {
            let n = if ops.len() > 1 { parse_number(&ops[1]).unwrap_or(1) } else { 1 };
            if n == 1 {
                insn.opcode = vec![if sz == 1 { 0xD0 } else { 0xD1 }];
            } else {
                insn.opcode = vec![if sz == 1 { 0xC0 } else { 0xC1 }];
                insn.imm = imm_bytes(n, 1);
            }
        }
        insn.set_reg_raw(*ext);
        set_rm(&mut insn, &dst)?;
        return insn.emit(addr);
    }

    // ---- x87 FPU ----------------------------------------------------------
    if let Some(bytes) = encode_x87(&mnem, &ops, resolve, &mut insn, addr)? {
        return Ok(bytes);
    }

    bail!("unsupported instruction: '{}'", line.trim())
}

/// Parses `st`, `st(0)`..`st(7)`.
fn parse_st(s: &str) -> Option<u8> {
    let t = s.trim().to_lowercase();
    if t == "st" {
        return Some(0);
    }
    let rest = t.strip_prefix("st")?;
    let rest = rest.trim();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    inner.trim().parse::<u8>().ok().filter(|n| *n < 8)
}

/// Encodes the x87 subset that appears in tables.
///
/// Returns `Ok(None)` when the mnemonic is not an x87 instruction.
fn encode_x87(
    mnem: &str,
    ops: &[String],
    resolve: Resolver,
    insn: &mut Insn,
    addr: u64,
) -> Result<Option<Vec<u8>>> {
    // No-operand / fixed-encoding forms.
    let fixed: &[(&str, &[u8])] = &[
        ("fldz", &[0xD9, 0xEE]),
        ("fld1", &[0xD9, 0xE8]),
        ("fldpi", &[0xD9, 0xEB]),
        ("fldl2e", &[0xD9, 0xEA]),
        ("fldlg2", &[0xD9, 0xEC]),
        ("fldln2", &[0xD9, 0xED]),
        ("fchs", &[0xD9, 0xE0]),
        ("fabs", &[0xD9, 0xE1]),
        ("ftst", &[0xD9, 0xE4]),
        ("fxam", &[0xD9, 0xE5]),
        ("fsqrt", &[0xD9, 0xFA]),
        ("fsin", &[0xD9, 0xFE]),
        ("fcos", &[0xD9, 0xFF]),
        ("frndint", &[0xD9, 0xFC]),
        ("fptan", &[0xD9, 0xF2]),
        ("fpatan", &[0xD9, 0xF3]),
        ("fyl2x", &[0xD9, 0xF1]),
        ("f2xm1", &[0xD9, 0xF0]),
        ("fscale", &[0xD9, 0xFD]),
        ("fprem", &[0xD9, 0xF8]),
        ("fincstp", &[0xD9, 0xF7]),
        ("fdecstp", &[0xD9, 0xF6]),
        ("fnop", &[0xD9, 0xD0]),
        ("fwait", &[0x9B]),
        ("fnclex", &[0xDB, 0xE2]),
        ("fninit", &[0xDB, 0xE3]),
        ("fcompp", &[0xDE, 0xD9]),
        ("fucompp", &[0xDA, 0xE9]),
        ("faddp", &[0xDE, 0xC1]),
        ("fmulp", &[0xDE, 0xC9]),
        ("fsubp", &[0xDE, 0xE9]),
        ("fsubrp", &[0xDE, 0xE1]),
        ("fdivp", &[0xDE, 0xF9]),
        ("fdivrp", &[0xDE, 0xF1]),
    ];
    if ops.is_empty() {
        if let Some((_, b)) = fixed.iter().find(|(n, _)| *n == mnem) {
            return Ok(Some(b.to_vec()));
        }
    }

    // fnstsw ax
    if mnem == "fnstsw" || mnem == "fstsw" {
        let prefix: &[u8] = if mnem == "fstsw" { &[0x9B] } else { &[] };
        if ops.first().map(|o| o.trim().eq_ignore_ascii_case("ax")).unwrap_or(false) {
            let mut v = prefix.to_vec();
            v.extend_from_slice(&[0xDF, 0xE0]);
            return Ok(Some(v));
        }
    }

    // Register-form st(i) instructions.
    let st_forms: &[(&str, u8, u8)] = &[
        // (mnemonic, opcode, base modrm)
        ("fld", 0xD9, 0xC0),
        ("fxch", 0xD9, 0xC8),
        ("fst", 0xDD, 0xD0),
        ("fstp", 0xDD, 0xD8),
        ("fucom", 0xDD, 0xE0),
        ("fucomp", 0xDD, 0xE8),
        ("free", 0xDD, 0xC0),
        ("ffree", 0xDD, 0xC0),
    ];
    if ops.len() == 1 {
        if let Some(i) = parse_st(&ops[0]) {
            if let Some((_, op, base)) = st_forms.iter().find(|(n, _, _)| *n == mnem) {
                return Ok(Some(vec![*op, base + i]));
            }
            // Arithmetic with st(i): fadd st(i) etc.
            let arith: &[(&str, u8)] = &[
                ("fadd", 0xC0),
                ("fmul", 0xC8),
                ("fsub", 0xE0),
                ("fsubr", 0xE8),
                ("fdiv", 0xF0),
                ("fdivr", 0xF8),
                ("fcom", 0xD0),
                ("fcomp", 0xD8),
            ];
            if let Some((_, base)) = arith.iter().find(|(n, _)| *n == mnem) {
                return Ok(Some(vec![0xD8, base + i]));
            }
            let arith_p: &[(&str, u8)] = &[
                ("faddp", 0xC0),
                ("fmulp", 0xC8),
                ("fsubp", 0xE8),
                ("fsubrp", 0xE0),
                ("fdivp", 0xF8),
                ("fdivrp", 0xF0),
            ];
            if let Some((_, base)) = arith_p.iter().find(|(n, _)| *n == mnem) {
                return Ok(Some(vec![0xDE, base + i]));
            }
        }
    }

    // Memory-form instructions: (mnemonic, /ext, opcode for m32, m64, m16, m80)
    struct MemForm {
        ext: u8,
        m32: Option<u8>,
        m64: Option<u8>,
        m16: Option<u8>,
    }
    let mem_form = |m: &str| -> Option<(MemForm, u8)> {
        // The second tuple element is the default size in bytes.
        Some(match m {
            "fld" => (MemForm { ext: 0, m32: Some(0xD9), m64: Some(0xDD), m16: None }, 4),
            "fst" => (MemForm { ext: 2, m32: Some(0xD9), m64: Some(0xDD), m16: None }, 4),
            "fstp" => (MemForm { ext: 3, m32: Some(0xD9), m64: Some(0xDD), m16: None }, 4),
            "fadd" => (MemForm { ext: 0, m32: Some(0xD8), m64: Some(0xDC), m16: None }, 4),
            "fmul" => (MemForm { ext: 1, m32: Some(0xD8), m64: Some(0xDC), m16: None }, 4),
            "fcom" => (MemForm { ext: 2, m32: Some(0xD8), m64: Some(0xDC), m16: None }, 4),
            "fcomp" => (MemForm { ext: 3, m32: Some(0xD8), m64: Some(0xDC), m16: None }, 4),
            "fsub" => (MemForm { ext: 4, m32: Some(0xD8), m64: Some(0xDC), m16: None }, 4),
            "fsubr" => (MemForm { ext: 5, m32: Some(0xD8), m64: Some(0xDC), m16: None }, 4),
            "fdiv" => (MemForm { ext: 6, m32: Some(0xD8), m64: Some(0xDC), m16: None }, 4),
            "fdivr" => (MemForm { ext: 7, m32: Some(0xD8), m64: Some(0xDC), m16: None }, 4),
            // Integer loads/stores: m32 uses DB, m64 uses DF /5, m16 uses DF.
            "fild" => (MemForm { ext: 0, m32: Some(0xDB), m64: None, m16: Some(0xDF) }, 4),
            "fistp" => (MemForm { ext: 3, m32: Some(0xDB), m64: None, m16: Some(0xDF) }, 4),
            "fist" => (MemForm { ext: 2, m32: Some(0xDB), m64: None, m16: Some(0xDF) }, 4),
            "fiadd" => (MemForm { ext: 0, m32: Some(0xDA), m64: None, m16: Some(0xDE) }, 4),
            "fimul" => (MemForm { ext: 1, m32: Some(0xDA), m64: None, m16: Some(0xDE) }, 4),
            "fisub" => (MemForm { ext: 4, m32: Some(0xDA), m64: None, m16: Some(0xDE) }, 4),
            "fidiv" => (MemForm { ext: 6, m32: Some(0xDA), m64: None, m16: Some(0xDE) }, 4),
            "fldcw" => (MemForm { ext: 5, m32: None, m64: None, m16: Some(0xD9) }, 2),
            "fnstcw" => (MemForm { ext: 7, m32: None, m64: None, m16: Some(0xD9) }, 2),
            _ => return None,
        })
    };

    if ops.len() == 1 {
        if let Some((form, default_size)) = mem_form(mnem) {
            let op = parse_operand(&ops[0], resolve)?;
            if let Op::M(ref m) = op {
                let size = m.size.unwrap_or(default_size);
                let opcode = match size {
                    2 => form.m16,
                    8 => form.m64,
                    _ => form.m32,
                }
                .or(form.m32)
                .ok_or_else(|| anyhow::anyhow!("{}: unsupported operand size {}", mnem, size))?;

                // fild/fistp with a qword operand use DF /5 and DF /7.
                let (opcode, ext) = if size == 8 && matches!(mnem, "fild" | "fistp") {
                    (0xDF, if mnem == "fild" { 5 } else { 7 })
                } else {
                    (opcode, form.ext)
                };

                insn.opcode = vec![opcode];
                insn.set_reg_raw(ext);
                set_rm(insn, &op)?;
                return Ok(Some(insn.emit(addr)?));
            }
        }
    }

    // fxsave / fxrstor
    if mnem == "fxsave" || mnem == "fxrstor" {
        if let Some(o) = ops.first() {
            let op = parse_operand(o, resolve)?;
            insn.opcode = vec![0x0F, 0xAE];
            insn.set_reg_raw(if mnem == "fxsave" { 0 } else { 1 });
            set_rm(insn, &op)?;
            return Ok(Some(insn.emit(addr)?));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<u64> {
        None
    }

    fn asm(line: &str) -> Vec<u8> {
        assemble(line, 0x1000, &none).unwrap_or_else(|e| panic!("{}: {}", line, e))
    }

    fn asm_at(line: &str, addr: u64, syms: &[(&str, u64)]) -> Vec<u8> {
        let f = |n: &str| {
            syms.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| *v)
        };
        assemble(line, addr, &f).unwrap_or_else(|e| panic!("{}: {}", line, e))
    }

    #[test]
    fn basic_mov_forms() {
        assert_eq!(asm("mov rcx,rsi"), vec![0x48, 0x89, 0xF1]);
        assert_eq!(asm("mov edx,[rbp+30]"), vec![0x8B, 0x55, 0x30]);
        assert_eq!(asm("mov rax,[r15+70]"), vec![0x49, 0x8B, 0x47, 0x70]);
        assert_eq!(asm("mov [r15+70],rax"), vec![0x49, 0x89, 0x47, 0x70]);
    }

    #[test]
    fn hex_is_the_default_radix() {
        // "30" must be 0x30, not decimal 30.
        assert_eq!(asm("mov edx,[rbp+30]")[2], 0x30);
        // "#500" is decimal 500 = 0x1F4, via the short B8+rd form.
        assert_eq!(asm("mov ecx,#500"), vec![0xB9, 0xF4, 0x01, 0x00, 0x00]);
        // Without "#", 500 is hex.
        assert_eq!(asm("mov ecx,500"), vec![0xB9, 0x00, 0x05, 0x00, 0x00]);
    }

    #[test]
    fn push_pop_and_flags() {
        assert_eq!(asm("push rax"), vec![0x50]);
        assert_eq!(asm("pop rax"), vec![0x58]);
        assert_eq!(asm("push r15"), vec![0x41, 0x57]);
        assert_eq!(asm("pushfq"), vec![0x9C]);
        assert_eq!(asm("popfq"), vec![0x9D]);
        assert_eq!(asm("nop"), vec![0x90]);
    }

    #[test]
    fn sse_load_and_store_directions() {
        // movdqu xmm0,[r15+70] -> F3 41 0F 6F 47 70 (matches the RDR2 original bytes)
        assert_eq!(asm("movdqu xmm0,[r15+70]"), vec![0xF3, 0x41, 0x0F, 0x6F, 0x47, 0x70]);
        // movups xmm3,[rcx+70] -> 0F 10 59 70
        assert_eq!(asm("movups xmm3,[rcx+70]"), vec![0x0F, 0x10, 0x59, 0x70]);
        // Store direction uses the other opcode.
        assert_eq!(asm("movss [rax+70],xmm0"), vec![0xF3, 0x0F, 0x11, 0x40, 0x70]);
        assert_eq!(asm("movss xmm0,[rax+70]"), vec![0xF3, 0x0F, 0x10, 0x40, 0x70]);
    }

    #[test]
    fn lea_and_original_rdr2_bytes() {
        // lea rcx,[rbp+00000080] -> 48 8D 8D 80 00 00 00
        assert_eq!(asm("lea rcx,[rbp+00000080]"), vec![0x48, 0x8D, 0x8D, 0x80, 0x00, 0x00, 0x00]);
        // lea r8,[rbp-50] -> 4C 8D 45 B0
        assert_eq!(asm("lea r8,[rbp-50]"), vec![0x4C, 0x8D, 0x45, 0xB0]);
    }

    #[test]
    fn branches_use_rel32() {
        // jmp forward to 0x1100 from 0x1000: rel = 0x1100 - 0x1005 = 0xFB
        assert_eq!(asm_at("jmp 1100", 0x1000, &[]), vec![0xE9, 0xFB, 0x00, 0x00, 0x00]);
        // jne is a 6-byte near form.
        let v = asm_at("jne code", 0x1000, &[("code", 0x1100)]);
        assert_eq!(v[0..2], [0x0F, 0x85]);
        assert_eq!(v.len(), 6);
        assert_eq!(i32::from_le_bytes([v[2], v[3], v[4], v[5]]), 0x1100 - 0x1006);
    }

    #[test]
    fn absolute_symbols_encode_rip_relative() {
        // mov [_pPos],r15 where _pPos = 0x2000, instruction at 0x1000.
        let v = asm_at("mov [_pPos],r15", 0x1000, &[("_pPos", 0x2000)]);
        assert_eq!(v[0..3], [0x4C, 0x89, 0x3D]);
        let disp = i32::from_le_bytes([v[3], v[4], v[5], v[6]]);
        assert_eq!(0x1000 + v.len() as i32 + disp, 0x2000);
    }

    #[test]
    fn ambiguous_memory_immediate_defaults_to_dword() {
        // cmp [_savePos],1 -> 83 /7 with imm8
        let v = asm_at("cmp [_savePos],1", 0x1000, &[("_savePos", 0x2000)]);
        assert_eq!(v[0], 0x83);
        assert_eq!(*v.last().unwrap(), 0x01);
        // mov [_savePos],0 -> C7 /0 with imm32
        let v = asm_at("mov [_savePos],0", 0x1000, &[("_savePos", 0x2000)]);
        assert_eq!(v[0], 0xC7);
        assert_eq!(v.len(), 10);
        // An explicit size specifier overrides the default.
        let v = asm_at("mov qword ptr [_savePos],0", 0x1000, &[("_savePos", 0x2000)]);
        assert_eq!(v[0], 0x48);
    }

    #[test]
    fn cmp_reg_with_absolute_memory() {
        // cmp r14,[_pPlayer]
        let v = asm_at("cmp r14,[_pPlayer]", 0x1000, &[("_pPlayer", 0x3000)]);
        assert_eq!(v[0..3], [0x4C, 0x3B, 0x35]);
    }

    #[test]
    fn sib_and_scaled_index() {
        // mov rax,[rbx+rcx*4+10]
        assert_eq!(asm("mov rax,[rbx+rcx*4+10]"), vec![0x48, 0x8B, 0x44, 0x8B, 0x10]);
        // rsp base forces a SIB byte.
        assert_eq!(asm("mov rax,[rsp+20]"), vec![0x48, 0x8B, 0x44, 0x24, 0x20]);
        // movaps [rsp+20],xmm1 -> 0F 29 4C 24 20
        assert_eq!(asm("movaps [rsp+20],xmm1"), vec![0x0F, 0x29, 0x4C, 0x24, 0x20]);
    }

    #[test]
    fn alu_and_test() {
        assert_eq!(asm("test rsi,rsi"), vec![0x48, 0x85, 0xF6]);
        assert_eq!(asm("xor ecx,ecx"), vec![0x31, 0xC9]);
        assert_eq!(asm("add rsp,28"), vec![0x48, 0x83, 0xC4, 0x28]);
        assert_eq!(asm("sub rsp,100"), vec![0x48, 0x81, 0xEC, 0x00, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn indirect_branches() {
        // call qword ptr [rax+40] -> FF 50 40
        assert_eq!(asm("call qword ptr [rax+40]"), vec![0xFF, 0x50, 0x40]);
        // call qword ptr [r8+08] -> 41 FF 50 08 (matches the table's disassembly)
        assert_eq!(asm("call qword ptr [r8+08]"), vec![0x41, 0xFF, 0x50, 0x08]);
        assert_eq!(asm("jmp rax"), vec![0xFF, 0xE0]);
        assert_eq!(asm("call [rbx]"), vec![0xFF, 0x13]);
        // A bare symbol is still a direct rel32 branch.
        let v = asm_at("call target", 0x1000, &[("target", 0x1100)]);
        assert_eq!(v[0], 0xE8);
        assert_eq!(v.len(), 5);
    }

    #[test]
    fn enter_and_align_support() {
        assert_eq!(asm("enter 0,0"), vec![0xC8, 0x00, 0x00, 0x00]);
        assert_eq!(asm("enter 8,0"), vec![0xC8, 0x08, 0x00, 0x00]);
        assert_eq!(asm("leave"), vec![0xC9]);
    }

    #[test]
    fn avx_vex_encodings() {
        // vmovss xmm0,[rax] -> C5 FA 10 00
        assert_eq!(asm("vmovss xmm0,[rax]"), vec![0xC5, 0xFA, 0x10, 0x00]);
        // vmovss [rax],xmm0 -> C5 FA 11 00
        assert_eq!(asm("vmovss [rax],xmm0"), vec![0xC5, 0xFA, 0x11, 0x00]);
        // vmovsd xmm1,[rcx] -> C5 FB 10 09
        assert_eq!(asm("vmovsd xmm1,[rcx]"), vec![0xC5, 0xFB, 0x10, 0x09]);
        // vxorps xmm0,xmm1,xmm2 -> C5 F0 57 C2
        assert_eq!(asm("vxorps xmm0,xmm1,xmm2"), vec![0xC5, 0xF0, 0x57, 0xC2]);
        // vaddss xmm0,xmm1,xmm2 -> C5 F2 58 C2
        assert_eq!(asm("vaddss xmm0,xmm1,xmm2"), vec![0xC5, 0xF2, 0x58, 0xC2]);
        // vmovups xmm0,[rax] -> C5 F8 10 00  (no implied prefix)
        assert_eq!(asm("vmovups xmm0,[rax]"), vec![0xC5, 0xF8, 0x10, 0x00]);
        // vmovdqu xmm0,[rax] -> C5 FA 6F 00
        assert_eq!(asm("vmovdqu xmm0,[rax]"), vec![0xC5, 0xFA, 0x6F, 0x00]);
        // An extended register forces the 3-byte form:
        // vmovss xmm0,[r8] -> C4 C1 7A 10 00
        assert_eq!(asm("vmovss xmm0,[r8]"), vec![0xC4, 0xC1, 0x7A, 0x10, 0x00]);
        // vcvtsi2ss xmm0,xmm1,eax -> C5 F2 2A C0
        assert_eq!(asm("vcvtsi2ss xmm0,xmm1,eax"), vec![0xC5, 0xF2, 0x2A, 0xC0]);
        // 64-bit source sets VEX.W, forcing 3 bytes:
        // vcvtsi2ss xmm0,xmm1,rax -> C4 E1 F2 2A C0
        assert_eq!(asm("vcvtsi2ss xmm0,xmm1,rax"), vec![0xC4, 0xE1, 0xF2, 0x2A, 0xC0]);
    }

    #[test]
    fn misc_and_string_instructions() {
        assert_eq!(asm("cld"), vec![0xFC]);
        assert_eq!(asm("sahf"), vec![0x9E]);
        assert_eq!(asm("pushad"), vec![0x60]);
        assert_eq!(asm("popad"), vec![0x61]);
        assert_eq!(asm("push eax"), vec![0x50]);
        assert_eq!(asm("pop edi"), vec![0x5F]);
        assert_eq!(asm("rep movsb"), vec![0xF3, 0xA4]);
        assert_eq!(asm("bswap eax"), vec![0x0F, 0xC8]);
        assert_eq!(asm("shufps xmm0,xmm1,4E"), vec![0x0F, 0xC6, 0xC1, 0x4E]);
    }

    #[test]
    fn x87_encodings() {
        // fld dword ptr [rax] -> D9 00
        assert_eq!(asm("fld dword ptr [rax]"), vec![0xD9, 0x00]);
        // fld qword ptr [rax] -> DD 00
        assert_eq!(asm("fld qword ptr [rax]"), vec![0xDD, 0x00]);
        // fstp dword ptr [rbx] -> D9 1B
        assert_eq!(asm("fstp dword ptr [rbx]"), vec![0xD9, 0x1B]);
        assert_eq!(asm("fldz"), vec![0xD9, 0xEE]);
        assert_eq!(asm("fld1"), vec![0xD9, 0xE8]);
        assert_eq!(asm("fld st(0)"), vec![0xD9, 0xC0]);
        assert_eq!(asm("fstp st(1)"), vec![0xDD, 0xD9]);
        assert_eq!(asm("fnstsw ax"), vec![0xDF, 0xE0]);
        // Unsized defaults to dword, matching typical table usage.
        assert_eq!(asm("fld [rax]"), vec![0xD9, 0x00]);
    }

    /// An unresolved symbol must be an error, never a silently-bogus operand.
    /// A script whose dependency is not loaded has to fail loudly, or it
    /// injects a compare against garbage and the feature quietly does nothing.
    /// The typed constants. RDR2's "Main CodeBase" uses `dd (float)1`, and
    /// failing to parse it aborted the master script that registers 35 symbols
    /// the rest of the table depends on.
    #[test]
    fn typed_float_constants() {
        assert_eq!(parse_number("(float)1"), Some(0x3F80_0000));
        assert_eq!(parse_number("(float)0"), Some(0));
        assert_eq!(parse_number("(float)1.5"), Some(0x3FC0_0000));
        assert_eq!(parse_number("(Float)1"), Some(0x3F80_0000));
        assert_eq!(parse_number("(double)1"), Some(0x3FF0_0000_0000_0000));
        // Still hex-by-default for plain numbers.
        assert_eq!(parse_number("10"), Some(0x10));
        assert_eq!(parse_number("(float)notanumber"), None);
    }

    /// `call kernel32.Sleep` used to assemble into a call to the current
    /// address when the export could not be resolved - a silent wrong answer.
    /// A Windows API is hundreds of GB from a game module, so the branch has
    /// to become an indirect call through a nearby pointer.
    #[test]
    fn far_calls_go_indirect_through_a_slot() {
        let f = |n: &str| (n.trim() == "kernel32.Sleep").then_some(0x7FFD_9FDD_B0E0u64);
        // Without a slot source it is an honest error.
        assert!(assemble("call kernel32.Sleep", 0x7FF7_4C79_0000, &f).is_err());

        // With one, it becomes `call qword ptr [rip+disp32]`, always 6 bytes.
        let slot_addr = 0x7FF7_4C79_1000u64;
        let far = |_t: u64| Some(slot_addr);
        let v = assemble_with("call kernel32.Sleep", 0x7FF7_4C79_0000, &f, &far).unwrap();
        assert_eq!(v.len(), 6);
        assert_eq!(v[0..2], [0xFF, 0x15]);
        let disp = i32::from_le_bytes([v[2], v[3], v[4], v[5]]) as i64;
        assert_eq!(0x7FF7_4C79_0000i64 + 6 + disp, slot_addr as i64);

        // jmp uses /4 rather than /2.
        let v = assemble_with("jmp kernel32.Sleep", 0x7FF7_4C79_0000, &f, &far).unwrap();
        assert_eq!(v[0..2], [0xFF, 0x25]);

        // In-range branches still use the compact rel32 form.
        let near = |n: &str| (n.trim() == "x").then_some(0x7FF7_4C79_0100u64);
        let v = assemble_with("call x", 0x7FF7_4C79_0000, &near, &far).unwrap();
        assert_eq!(v[0], 0xE8);
        assert_eq!(v.len(), 5);
    }

    /// Five gaps found by diffing against the disassembly tables embed over 90k
    /// instructions from the table corpus.
    #[test]
    fn spells_low_bytes_of_extended_registers_as_r8l() {
        // r8l/r13l/r14l are the spelling of r8b/r13b/r14b.
        assert_eq!(asm("mov r8l,1"), asm("mov r8b,1"));
        assert_eq!(asm("mov r13l,al"), asm("mov r13b,al"));
        assert_eq!(asm("mov r14l,al"), asm("mov r14b,al"));
        // `mov r8b,01` -> 41 B0 01
        assert_eq!(asm("mov r8l,1"), vec![0x41, 0xB0, 0x01]);
    }

    #[test]
    fn short_and_near_branch_hints_are_accepted() {
        let f = |n: &str| (n.trim() == "target").then_some(0x1100u64);
        let plain = assemble("jmp target", 0x1000, &f).unwrap();
        assert_eq!(assemble("jmp short target", 0x1000, &f).unwrap(), plain);
        assert_eq!(assemble("jmp near target", 0x1000, &f).unwrap(), plain);
        assert_eq!(assemble("je short target", 0x1000, &f).unwrap(),
                   assemble("je target", 0x1000, &f).unwrap());
    }

    #[test]
    fn hyphenated_module_names_are_not_subtraction() {
        // `Palworld-Win64-Shipping.exe+1234` is one symbol, not a-b-c.
        let f = |n: &str| {
            (n.trim() == "Palworld-Win64-Shipping.exe+1234").then_some(0x1_4000_1234u64)
        };
        let v = assemble("jmp Palworld-Win64-Shipping.exe+1234", 0x1_4000_0000, &f).unwrap();
        assert_eq!(v[0], 0xE9);
        let rel = i32::from_le_bytes([v[1], v[2], v[3], v[4]]) as i64;
        assert_eq!(0x1_4000_0000i64 + 5 + rel, 0x1_4000_1234);
        // Genuine subtraction still works.
        assert_eq!(asm("mov rax,[rbp-10]"), vec![0x48, 0x8B, 0x45, 0xF0]);
    }

    #[test]
    fn segment_prefixed_operands() {
        // mov rax,fs:[30] -> 64 48 8B 04 25 30 00 00 00
        assert_eq!(
            asm("mov rax,fs:[30]"),
            vec![0x64, 0x48, 0x8B, 0x04, 0x25, 0x30, 0x00, 0x00, 0x00]
        );
        // gs uses 65, and the bracket-less form means the same thing.
        assert_eq!(asm("mov rax,gs:[30]")[0], 0x65);
        assert_eq!(asm("mov rax,fs:30"), asm("mov rax,fs:[30]"));
    }

    #[test]
    fn multi_byte_nop_padding() {
        // nop dword ptr [rax] -> 0F 1F 00
        assert_eq!(asm("nop dword ptr [rax]"), vec![0x0F, 0x1F, 0x00]);
        // 16-bit form takes the operand-size prefix.
        assert_eq!(asm("nop word ptr [rax]"), vec![0x66, 0x0F, 0x1F, 0x00]);
        // Bare nop and `nop N` padding are unaffected.
        assert_eq!(asm("nop"), vec![0x90]);
    }

    #[test]
    fn unresolved_branch_targets_are_errors() {
        assert!(assemble("call kernel32.Sleep", 0x1000, &none).is_err());
        assert!(assemble("jmp someUnknownLabel", 0x1000, &none).is_err());
        assert!(assemble("je notDefinedAnywhere", 0x1000, &none).is_err());
        // Resolvable targets still assemble, and a numeric one needs no symbol.
        let f = |n: &str| (n.trim() == "target").then_some(0x1100u64);
        assert!(assemble("call target", 0x1000, &f).is_ok());
        assert!(assemble("jmp 1100", 0x1000, &none).is_ok());
    }

    #[test]
    fn unresolved_symbols_are_errors() {
        // `cmp r14,[_pPlayer]` where _pPlayer is registered by another script.
        assert!(assemble("cmp r14,[_pPlayer]", 0x1000, &none).is_err());
        assert!(assemble("mov [_pPos],r15", 0x1000, &none).is_err());
        assert!(assemble("mov rax,[_unknownSym+10]", 0x1000, &none).is_err());
        // Resolving it makes the same line assemble.
        let f = |n: &str| (n.trim() == "_pPlayer").then_some(0x7FF6_0100_0000u64);
        assert!(assemble("cmp r14,[_pPlayer]", 0x7FF6_0100_0000, &f).is_ok());
    }

    #[test]
    fn unsupported_instructions_are_reported() {
        // Silently skipping unknown lines is what made bad injections look successful.
        assert!(assemble("vfmadd213ps xmm0,xmm1,xmm2", 0x1000, &none).is_err());
        assert!(assemble("mov [rax],", 0x1000, &none).is_err());
    }

    /// The five casts.
    ///
    /// `(int)` is the one that matters most in practice (566 uses in the
    /// corpus) and the one easiest to get wrong: it is parsed with
    /// `StrToQWordEx`, so it is **decimal**, unlike every other bare number in
    /// an AA script. Reading `(int)100` as hex writes a plausible wrong value
    /// instead of failing, which is the worst kind of bug here.
    #[test]
    fn numeric_casts() {
        assert_eq!(parse_number("(int)100"), Some(100), "(int) is decimal, not hex");
        assert_eq!(parse_number("(int)0"), Some(0));
        assert_eq!(parse_number("(INT)464"), Some(464), "case-insensitive");

        assert_eq!(parse_number("(float)1.0"), Some(0x3F80_0000));
        assert_eq!(parse_number("(double)1.0"), Some(0x3FF0_0000_0000_0000u64 as i64));

        // The halves of a double, split for 32-bit writes.
        let bits = 1.0f64.to_bits();
        assert_eq!(parse_number("(double32l)1.0"), Some((bits & 0xFFFF_FFFF) as i64));
        assert_eq!(parse_number("(double32h)1.0"), Some((bits >> 32) as i64));

        // `(double32l)` must not be swallowed by the `(double)` arm.
        assert_ne!(parse_number("(double32l)1.0"), parse_number("(double)1.0"));

        // Not recognised casts - must stay unparsed rather than guessed at.
        assert_eq!(parse_number("(byte)5"), None);
        assert_eq!(parse_number("(dword)5"), None);
    }

}

/// 32-bit mode. The syntax of a 32-bit table is the one CE's assembler
/// accepts for an IA-32 target: the expected bytes below are what it emits.
#[cfg(test)]
mod bits32_tests {
    use super::*;

    fn asm32(line: &str, addr: u64, syms: &[(&str, u64)]) -> Result<Vec<u8>> {
        let f = |n: &str| {
            syms.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| *v)
        };
        assemble_mode(Mode::Bits32, line, addr, &f, &|_| None)
    }

    fn ok32(line: &str) -> Vec<u8> {
        asm32(line, 0x0040_1000, &[]).unwrap_or_else(|e| panic!("{}: {}", line, e))
    }

    /// `mov eax,[00A3B5C0]` is `A1`/`8B 05` + disp32, not RIP-relative: the
    /// bytes must be the address itself, wherever the instruction sits.
    #[test]
    fn absolute_operands_are_plain_disp32() {
        let v = asm32("mov eax,[_pPlayer]", 0x0040_1000, &[("_pPlayer", 0x00A3_B5C0)]).unwrap();
        assert_eq!(v, vec![0x8B, 0x05, 0xC0, 0xB5, 0xA3, 0x00]);
        // Same bytes from a different address: no RIP arithmetic.
        let w = asm32("mov eax,[_pPlayer]", 0x1000_0000, &[("_pPlayer", 0x00A3_B5C0)]).unwrap();
        assert_eq!(v, w);
        // Store direction and immediate-to-memory follow the same rule.
        assert_eq!(
            asm32("mov [_hp],ecx", 0x0040_1000, &[("_hp", 0x0050_0000)]).unwrap(),
            vec![0x89, 0x0D, 0x00, 0x00, 0x50, 0x00]
        );
        assert_eq!(
            asm32("mov [_hp],1", 0x0040_1000, &[("_hp", 0x0050_0000)]).unwrap(),
            vec![0xC7, 0x05, 0x00, 0x00, 0x50, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
    }

    /// The common 32-bit hook shapes: no REX anywhere.
    #[test]
    fn classic_32bit_instructions() {
        assert_eq!(ok32("mov eax,[ebp+08]"), vec![0x8B, 0x45, 0x08]);
        assert_eq!(ok32("mov [esi+2C],eax"), vec![0x89, 0x46, 0x2C]);
        assert_eq!(ok32("mov eax,[esp+04]"), vec![0x8B, 0x44, 0x24, 0x04]);
        assert_eq!(ok32("push eax"), vec![0x50]);
        assert_eq!(ok32("pop edi"), vec![0x5F]);
        assert_eq!(ok32("pushad"), vec![0x60]);
        assert_eq!(ok32("fld dword ptr [esi+10]"), vec![0xD9, 0x46, 0x10]);
        assert_eq!(ok32("movss [edi+14],xmm0"), vec![0xF3, 0x0F, 0x11, 0x47, 0x14]);
        assert_eq!(ok32("cmp dword ptr [eax],0"), vec![0x83, 0x38, 0x00]);
        assert_eq!(ok32("lea ecx,[ebx+edx*4+10]"), vec![0x8D, 0x4C, 0x93, 0x10]);
        assert_eq!(ok32("call dword ptr [eax+40]"), vec![0xFF, 0x50, 0x40]);
        assert_eq!(ok32("mov ecx,#500"), vec![0xB9, 0xF4, 0x01, 0x00, 0x00]);
        assert_eq!(ok32("ret"), vec![0xC3]);
    }

    /// Branches wrap at 4GB: a jump across the whole address space is still a
    /// rel32, and the far-pointer fallback is never consulted.
    #[test]
    fn branches_always_reach() {
        let v = asm32("jmp 1100", 0x1000, &[]).unwrap();
        assert_eq!(v, vec![0xE9, 0xFB, 0x00, 0x00, 0x00]);
        // 0x00401000 -> 0xFFFF0000: out of ±2GB as a signed distance, but
        // the CPU wraps it, so rel = target - next (mod 2^32).
        let v = asm32("jmp FFFF0000", 0x0040_1000, &[]).unwrap();
        assert_eq!(v[0], 0xE9);
        let rel = u32::from_le_bytes([v[1], v[2], v[3], v[4]]);
        assert_eq!(0x0040_1005u32.wrapping_add(rel), 0xFFFF_0000);
        let v = asm32("jne FFFF0000", 0x0040_1000, &[]).unwrap();
        assert_eq!(v[0..2], [0x0F, 0x85]);
        let rel = u32::from_le_bytes([v[2], v[3], v[4], v[5]]);
        assert_eq!(0x0040_1006u32.wrapping_add(rel), 0xFFFF_0000);
        // Same for the system call path: kernel32 is in reach without a slot.
        let v = asm32("call kernel32.Sleep", 0x0040_1000, &[("kernel32.Sleep", 0x7C80_2446)])
            .unwrap();
        assert_eq!(v[0], 0xE8);
        assert_eq!(v.len(), 5);
    }

    /// A 64-bit register in a 32-bit table is a table bug, and silently
    /// encoding it as its 32-bit half would corrupt the game.
    #[test]
    fn x86_64_only_forms_are_rejected() {
        for line in [
            "mov rax,rcx",
            "mov rax,[ebp+08]",
            "mov eax,[rbx+08]",
            "mov eax,[ebx+rcx*4]",
            "push rax",
            "pop r8",
            "mov r8d,eax",
            "mov r8l,1",
            "mov sil,al",
            "movsxd eax,[ebx]",
            "cqo",
            "cdqe",
            "movsq",
            "stosq",
            "vmovss xmm8,[eax]",
            "cvtsi2ss xmm0,rax",
            "mov rax,123456789AB",
        ] {
            assert!(asm32(line, 0x0040_1000, &[]).is_err(), "{} must be rejected", line);
        }
        // ah/ch/dh/bh are fine: they never needed a REX.
        assert_eq!(ok32("mov ah,al"), vec![0x88, 0xC4]);
        // An absolute address above 4GB cannot belong to a 32-bit process.
        assert!(asm32("mov eax,[big]", 0x0040_1000, &[("big", 0x1_0000_0000)]).is_err());
    }

    /// The default mode is unchanged: every existing 64-bit caller keeps the
    /// same bytes.
    #[test]
    fn bits64_is_the_default() {
        let f = |_: &str| None;
        assert_eq!(
            assemble("mov rcx,rsi", 0x1000, &f).unwrap(),
            assemble_mode(Mode::Bits64, "mov rcx,rsi", 0x1000, &f, &|_| None).unwrap()
        );
        assert_eq!(Mode::for_target(true), Mode::Bits64);
        assert_eq!(Mode::for_target(false), Mode::Bits32);
    }

    /// `rep movsb` recurses into the inner instruction; the mode has to
    /// survive the recursion.
    #[test]
    fn prefixed_instructions_keep_the_mode() {
        assert_eq!(ok32("rep movsb"), vec![0xF3, 0xA4]);
        assert!(asm32("rep movsq", 0x0040_1000, &[]).is_err());
    }
}

#[cfg(test)]
mod self_overlap_tests {
    use super::*;

    /// The exact case `--verify-asm` surfaced on rdr2.CT: this is encoded as
    /// `EB 02`, and the rel32 form would be `E9 FF FF FF FF` - a jump to its
    /// own fifth byte.
    #[test]
    fn a_jump_into_its_own_encoding_is_rejected() {
        let err = assemble("jmp 1004", 0x1000, &|_| None).unwrap_err().to_string();
        assert!(err.contains("inside its own"), "unexpected error: {}", err);
    }

    /// One byte past the encoding is the first legal target, and must still
    /// assemble - the guard has to reject overlap, not short jumps generally.
    #[test]
    fn a_jump_just_past_its_own_encoding_is_fine() {
        assert_eq!(
            assemble("jmp 1005", 0x1000, &|_| None).unwrap(),
            vec![0xE9, 0x00, 0x00, 0x00, 0x00]
        );
    }

    /// A conditional branch is six bytes, so its overlap window is one wider.
    #[test]
    fn a_conditional_branch_into_its_own_encoding_is_rejected() {
        assert!(assemble("je 1005", 0x1000, &|_| None).is_err());
        assert!(assemble("je 1006", 0x1000, &|_| None).is_ok());
    }

    /// Backward branches always land outside the instruction and must be
    /// untouched by the guard.
    #[test]
    fn a_backward_branch_is_unaffected() {
        assert!(assemble("jmp 0FF0", 0x1000, &|_| None).is_ok());
    }
}
