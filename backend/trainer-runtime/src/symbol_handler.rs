use crate::process::GameProcess;
use anyhow::{bail, Result};
use std::collections::HashMap;

#[derive(Default)]
pub struct SymbolHandler {
    user_symbols: HashMap<String, usize>,
    /// One frame per Auto Assembler section currently executing, holding the
    /// names *that section* created with `alloc` or `aobscan*`.
    ///
    /// These are deliberately not global. A script's allocations and scan
    /// results live in the script's own symbol list and only `registersymbol`
    /// copies one into the shared table, so two scripts
    /// can both call their scratch block `newmem` without ever meeting.
    ///
    /// This engine used to register them globally, and that is what killed RDR2:
    /// `BASEWEATHER` allocated `newmem`, and when `Main CodeBase` ran afterwards
    /// its own `newmem:` line *resolved* — so the rule in `collect_labels` read
    /// it as an address setter and wrote 366 lines of code on top of
    /// BASEWEATHER's live, already-hooked allocation. The game jumped into it
    /// and died about a second later. Enabling `Main CodeBase` first made the
    /// same table work, because then nothing called `newmem` existed yet.
    ///
    /// A stack rather than one frame: a `{$lua}` block can call `autoAssemble()`
    /// on another script mid-section, and that inner section's allocations must
    /// not leak into the outer one either.
    scopes: Vec<HashMap<String, usize>>,
}

impl SymbolHandler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a name every script can see. `registersymbol`'s job, and the
    /// only way a table shares an address between scripts.
    pub fn register_symbol(&mut self, name: &str, addr: usize) {
        self.user_symbols.insert(name.to_lowercase(), addr);
    }

    /// Opens a symbol scope for one Auto Assembler section.
    pub fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    /// Closes it, discarding every name the section did not `registersymbol`.
    pub fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Registers a name visible only to the section that created it.
    ///
    /// Falls back to the global table when no section is executing, so a caller
    /// that forgets to open a scope still resolves rather than silently losing
    /// the symbol.
    pub fn register_local(&mut self, name: &str, addr: usize) {
        match self.scopes.last_mut() {
            Some(scope) => {
                scope.insert(name.to_lowercase(), addr);
            }
            None => {
                self.user_symbols.insert(name.to_lowercase(), addr);
            }
        }
    }

    /// `registersymbol(x)`: promotes a section-local name into the global table.
    ///
    /// Returns false if the name is not one of this section's own — the caller
    /// then has a label address for it, or nothing at all.
    pub fn promote(&mut self, name: &str) -> bool {
        let key = name.to_lowercase();
        let found = self
            .scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&key).copied());
        match found {
            Some(addr) => {
                self.user_symbols.insert(key, addr);
                true
            }
            None => false,
        }
    }

    /// Every *global* symbol, for the debug endpoint. Section-locals are not
    /// included: they exist only while that section is running.
    pub fn all_symbols(&self) -> Vec<(String, usize)> {
        self.user_symbols.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    pub fn unregister_symbol(&mut self, name: &str) {
        self.user_symbols.remove(&name.to_lowercase());
    }

    /// Innermost section scope first, then the global table.
    fn lookup(&self, key: &str) -> Option<usize> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(key).copied())
            .or_else(|| self.user_symbols.get(key).copied())
    }

    pub fn get_address_from_name(&self, name: &str, process: Option<&GameProcess>) -> Result<usize> {
        // Module names are written quoted, e.g. `"kenshi_x64.exe"+02133FA0`.
        // `trim_matches` only strips the *outer* quotes, which leaves a trailing
        // quote glued to the module name in the `module+offset` form, so the
        // lookup silently fails. Drop quotes outright instead.
        let cleaned = name.trim().replace('"', "");
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            bail!("Empty symbol name");
        }

        let key = trimmed.to_lowercase();

        // 1. This section's own allocations and scan results, then the symbols
        //    tables share with `registersymbol`.
        if let Some(addr) = self.lookup(&key) {
            return Ok(addr);
        }

        // 2. Check if hex address literal
        if let Ok(addr) = usize::from_str_radix(trimmed.trim_start_matches("0x").trim_start_matches('$'), 16) {
            return Ok(addr);
        }

        // 3. Check for module+offset (e.g. "game.exe+1A2B" or "mono.dll+0x40")
        if let Some((mod_part, off_part)) = split_module_offset(trimmed) {

            // The left side may be a module name or a symbol an Auto Assembler
            // script owns (e.g. `_pPos+70`, or this section's own `newmem+10`).
            let base = match self.lookup(&mod_part.to_lowercase()) {
                Some(addr) => addr,
                None => match process {
                    Some(p) => p.get_module_base(mod_part).ok_or_else(|| {
                        anyhow::anyhow!("Module '{}' not found in process", mod_part)
                    })?,
                    None => bail!("Cannot resolve '{}' without a process", mod_part),
                },
            };

            let offset = usize::from_str_radix(off_part.trim_start_matches("0x").trim_start_matches('$'), 16)
                .or_else(|_| off_part.parse::<usize>())?;

            return Ok(base + offset);
        }

        // 4. Exported function, e.g. `kernel32.Sleep`. Split at the last dot so
        //    `kernel32.dll.Sleep` works as well as `kernel32.Sleep`.
        if let Some(p) = process {
            if let Some(dot) = trimmed.rfind('.') {
                let (module, export) = (&trimmed[..dot], &trimmed[dot + 1..]);
                if !module.is_empty() && !export.is_empty() {
                    if let Some(addr) = p.get_export(module, export) {
                        return Ok(addr);
                    }
                }
            }
        }

        // 5. Try resolving module name alone
        if let Some(p) = process {
            if let Some(base) = p.get_module_base(trimmed) {
                return Ok(base);
            }
        }

        bail!("Could not resolve symbol '{}'", name)
    }
}

/// Splits a `module+offset` expression into its two halves.
fn split_module_offset(s: &str) -> Option<(&str, &str)> {
    let pos = s.find('+')?;
    let module = s[..pos].trim();
    let offset = s[pos + 1..].trim();
    if module.is_empty() || offset.is_empty() {
        return None;
    }
    Some((module, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_module_names_do_not_leak_quotes() {
        // Kenshi's table uses `"kenshi_x64.exe"+02133FA0`. A trailing quote
        // glued to the module name makes every record in that table unresolvable.
        let cleaned = "\"kenshi_x64.exe\"+02133FA0".replace('"', "");
        let (m, o) = split_module_offset(&cleaned).unwrap();
        assert_eq!(m, "kenshi_x64.exe");
        assert_eq!(o, "02133FA0");
    }

    #[test]
    fn unquoted_and_dll_forms() {
        assert_eq!(split_module_offset("game.exe+1A2B"), Some(("game.exe", "1A2B")));
        assert_eq!(split_module_offset("mono.dll+0x40"), Some(("mono.dll", "0x40")));
        assert_eq!(split_module_offset("noplus"), None);
    }

    /// The RDR2 crash, in miniature.
    ///
    /// `BASEWEATHER` does `alloc(newmem,…)`; `Main CodeBase` runs next and has
    /// its own `newmem:` line. `collect_labels` reads "already resolves" as
    /// "this is an address to write code at", so if the first script's name is
    /// still visible the second writes its whole block on top of live, hooked
    /// code. Measured: the game died about a second later.
    #[test]
    fn one_scripts_allocation_is_invisible_to_the_next() {
        let mut s = SymbolHandler::new();

        s.push_scope();
        s.register_local("newmem", 0x13FF8_0000);
        assert_eq!(s.get_address_from_name("newmem", None).unwrap(), 0x13FF8_0000);
        s.pop_scope();

        s.push_scope();
        assert!(
            s.get_address_from_name("newmem", None).is_err(),
            "another script's newmem must not resolve, or its `newmem:` line stops being a label"
        );
        s.pop_scope();
    }

    /// `registersymbol` is the only bridge, and it has to work for a name that
    /// came from `alloc` rather than from a label - `coord2` in RDR2 is both.
    #[test]
    fn registersymbol_is_what_makes_a_name_shared() {
        let mut s = SymbolHandler::new();

        s.push_scope();
        s.register_local("coord2", 0x13FE5_0000);
        assert!(s.promote("coord2"));
        s.pop_scope();

        assert_eq!(s.get_address_from_name("coord2", None).unwrap(), 0x13FE5_0000);
    }

    #[test]
    fn promoting_a_name_the_section_never_created_reports_failure() {
        let mut s = SymbolHandler::new();
        s.push_scope();
        assert!(!s.promote("never_allocated"));
        s.pop_scope();
    }

    /// A `{$lua}` block can call `autoAssemble()` on another script mid-section,
    /// so scopes nest: the inner one sees outward, the outer never sees in.
    #[test]
    fn nested_sections_see_outward_only() {
        let mut s = SymbolHandler::new();
        s.push_scope();
        s.register_local("outer", 0x1000);

        s.push_scope();
        s.register_local("inner", 0x2000);
        assert_eq!(s.get_address_from_name("outer", None).unwrap(), 0x1000);
        s.pop_scope();

        assert!(s.get_address_from_name("inner", None).is_err());
        s.pop_scope();
    }

    /// Section-locals shadow globals while the section runs - a script that
    /// allocs `newmem` means *its* newmem, even if another script shared one.
    #[test]
    fn a_sections_own_name_wins_over_a_shared_one() {
        let mut s = SymbolHandler::new();
        s.register_symbol("newmem", 0xAAAA);

        s.push_scope();
        s.register_local("newmem", 0xBBBB);
        assert_eq!(s.get_address_from_name("newmem", None).unwrap(), 0xBBBB);
        s.pop_scope();

        assert_eq!(s.get_address_from_name("newmem", None).unwrap(), 0xAAAA);
    }

    #[test]
    fn registered_symbols_resolve_case_insensitively() {
        let mut s = SymbolHandler::new();
        s.register_symbol("_pPos", 0x1234);
        assert_eq!(s.get_address_from_name("_ppos", None).unwrap(), 0x1234);
        assert_eq!(s.get_address_from_name("_pPos", None).unwrap(), 0x1234);
    }
}
