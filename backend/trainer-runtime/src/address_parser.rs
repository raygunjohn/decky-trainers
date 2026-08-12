//! Evaluates table address expressions.
//!
//! Supports the forms that appear in `<Address>` fields:
//!
//! - `"game.exe"+1A2B` - module base plus offset
//! - `_pPos` - a symbol registered by an Auto Assembler script
//! - `[coord2]+80` - dereference a pointer, then add an offset
//! - `[[base]+10]+20` - nested dereferences
//!
//! Numbers are hex by default, matching table convention.

use crate::process::GameProcess;
use crate::symbol_handler::SymbolHandler;
use anyhow::{bail, Result};

pub fn evaluate_address(
    expr: &str,
    process: Option<&GameProcess>,
    symbols: &SymbolHandler,
) -> Result<usize> {
    let expr = expr.trim();
    if expr.is_empty() {
        bail!("Empty address expression");
    }

    // A whole-expression symbol lookup takes priority, so quoted module names
    // containing '+' (e.g. `"game.exe"+10`) are handled by the symbol handler.
    if let Ok(addr) = symbols.get_address_from_name(expr, process) {
        return Ok(addr);
    }

    let mut total: i64 = 0;
    let mut sign: i64 = 1;
    let mut i = 0;
    let chars: Vec<char> = expr.chars().collect();
    let mut saw_term = false;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        if c == '+' || c == '-' {
            sign = if c == '-' { -1 } else { 1 };
            i += 1;
            continue;
        }

        if c == '[' {
            // Find the matching close bracket.
            let mut depth = 0i32;
            let mut end = None;
            for (j, ch) in chars.iter().enumerate().skip(i) {
                match ch {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(j);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let end = end.ok_or_else(|| anyhow::anyhow!("Unbalanced '[' in '{}'", expr))?;
            let inner: String = chars[i + 1..end].iter().collect();

            let inner_addr = evaluate_address(&inner, process, symbols)?;
            let p = process
                .ok_or_else(|| anyhow::anyhow!("Process required to dereference '{}'", expr))?;

            let mut buf = if p.is_64bit { vec![0u8; 8] } else { vec![0u8; 4] };
            p.read_memory(inner_addr, &mut buf)
                .map_err(|_| anyhow::anyhow!("Cannot read pointer at {:#X}", inner_addr))?;

            let ptr = if p.is_64bit {
                u64::from_le_bytes(buf.try_into().unwrap()) as i64
            } else {
                u32::from_le_bytes(buf.try_into().unwrap()) as i64
            };

            total += sign * ptr;
            saw_term = true;
            i = end + 1;
            continue;
        }

        // Plain term: read up to the next top-level +/-.
        let start = i;
        while i < chars.len() && chars[i] != '+' && chars[i] != '-' && chars[i] != '[' {
            i += 1;
        }
        let term: String = chars[start..i].iter().collect();
        let term = term.trim();
        if term.is_empty() {
            continue;
        }

        let val = if let Ok(v) = symbols.get_address_from_name(term, process) {
            v as i64
        } else if let Some(v) = parse_hex_or_dec(term) {
            v
        } else {
            bail!("Failed to resolve '{}' in address expression '{}'", term, expr);
        };

        total += sign * val;
        saw_term = true;
    }

    if !saw_term {
        bail!("Address expression '{}' produced no value", expr);
    }

    Ok(total as usize)
}

/// Parses a table numeric literal. Hex by default; `#` forces decimal.
fn parse_hex_or_dec(s: &str) -> Option<i64> {
    let t = s.trim().trim_matches('"');
    if t.is_empty() {
        return None;
    }
    if let Some(r) = t.strip_prefix('#') {
        return r.trim().parse::<i64>().ok();
    }
    let t2 = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .or_else(|| t.strip_prefix('$'))
        .unwrap_or(t);
    u64::from_str_radix(t2, 16).ok().map(|v| v as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_hex_by_default() {
        assert_eq!(parse_hex_or_dec("10"), Some(0x10));
        assert_eq!(parse_hex_or_dec("#10"), Some(10));
        assert_eq!(parse_hex_or_dec("0x1F"), Some(0x1F));
        assert_eq!(parse_hex_or_dec("$FF"), Some(0xFF));
    }

    #[test]
    fn plain_symbol_and_offset_arithmetic() {
        let mut s = SymbolHandler::new();
        s.register_symbol("_pPos", 0x1000);
        assert_eq!(evaluate_address("_pPos", None, &s).unwrap(), 0x1000);
        assert_eq!(evaluate_address("_pPos+70", None, &s).unwrap(), 0x1070);
        assert_eq!(evaluate_address("_pPos-10", None, &s).unwrap(), 0x0FF0);
    }

    #[test]
    fn dereference_requires_a_process() {
        let s = SymbolHandler::new();
        // `[coord2]+80` must not be mistaken for a plain symbol.
        assert!(evaluate_address("[coord2]+80", None, &s).is_err());
    }
}
