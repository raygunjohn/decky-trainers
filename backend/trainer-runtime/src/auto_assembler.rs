//! Auto Assembler script engine.
//!
//! Execution mirrors the Auto Assembler:
//!
//! 1. **Preprocess** - strip comments and `{...}` blocks, expand `define()`s,
//!    and reject Lua-only scripts up front.
//! 2. **Directives** - run `aobscanmodule`/`aobscan`, `alloc`, `label`,
//!    `registersymbol`, `assert`. Allocations are placed near the injection
//!    site so a `jmp rel32` can reach them.
//! 3. **Layout pass** - walk the code assigning addresses to every label
//!    without writing anything. Encoded instruction lengths never depend on a
//!    symbol's value, so sizes computed here are final.
//! 4. **Emit pass** - assemble again with every label known and write to the
//!    target process, saving original bytes so `[DISABLE]` can restore them.
//!
//! An unrecognised instruction is a hard error. The previous implementation
//! silently ignored any line it did not understand, so a script could report
//! success while injecting nothing at all - leaving symbols like `_pPos`
//! pointing at freshly allocated, zeroed memory.

use crate::aob_scanner::{aob_scan, parse_aob_pattern};
use crate::assembler::{assemble_with, parse_number};
use crate::process::GameProcess;
use crate::lua_engine::{ExecEnv, LuaCtx, RecordView};
use crate::symbol_handler::SymbolHandler;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct AutoAssemblerScript {
    pub raw_script: String,
    pub enable_lines: Vec<String>,
    pub disable_lines: Vec<String>,
    /// Memory allocated by `[ENABLE]`, freed on `[DISABLE]`.
    allocations: HashMap<String, (usize, usize)>,
    /// Original bytes overwritten by `[ENABLE]`, restored on `[DISABLE]`.
    original_bytes: Vec<(usize, Vec<u8>)>,
    /// Symbols this script registered, so they can be removed cleanly.
    registered: Vec<String>,
    /// True when the script body is Lua rather than assembler.
    pub is_lua: bool,
}

/// Pointer slots for branches that cannot be reached with a rel32.
///
/// Assigned deterministically (same target -> same slot), and the cursor is
/// reset before each pass so the layout and emit passes agree.
#[derive(Default)]
struct FarPool {
    base: usize,
    size: usize,
    next: std::cell::Cell<usize>,
    map: std::cell::RefCell<HashMap<u64, u64>>,
}

impl FarPool {
    fn reset(&self) {
        self.next.set(0);
        self.map.borrow_mut().clear();
    }

    /// Address of a slot holding `target`, or None when the pool is exhausted.
    fn slot(&self, target: u64) -> Option<u64> {
        if self.base == 0 {
            return None;
        }
        if let Some(&a) = self.map.borrow().get(&target) {
            return Some(a);
        }
        let off = self.next.get();
        if off + 8 > self.size {
            return None;
        }
        self.next.set(off + 8);
        let addr = (self.base + off) as u64;
        self.map.borrow_mut().insert(target, addr);
        Some(addr)
    }

    /// Writes every assigned slot into the target process.
    fn flush(&self, process: &GameProcess) {
        for (target, addr) in self.map.borrow().iter() {
            let _ = process.write_memory(*addr as usize, &target.to_le_bytes());
        }
    }
}

/// One chunk of code destined for a specific address.
struct Block {
    addr: u64,
    /// (source line, assembled length)
    lines: Vec<(String, usize)>,
}

impl AutoAssemblerScript {
    pub fn parse(script: &str) -> Self {
        // Lines before the first `[ENABLE]`/`[DISABLE]` marker belong to *both*
        // sections, and dropping them breaks a whole family of tables.
        //
        // the `getEnableOrDisableScript` is explicit
        // about it - "removes the enable or disable section from a script
        // leaving only the **outer code** and the selected script routine". It
        // keeps every line outside both sections plus the chosen one, and
        // strips the marker lines themselves.
        //
        // The pattern that needs it is common (Otis_Inf / SunBeam tables, and
        // rdr2.CT's BASETIME): a `{$lua}` preamble defines helper functions,
        // then the section body is a single `return someFunction()`. the
        // `luacode` scans `{$lua}` to `{$asm}` *or the end of the script*, so
        // once the preamble and the body are one script the whole thing is one
        // Lua block and the `return` is Lua, not assembler.
        //
        // Discarding the preamble left `return obtainAddresses()` standing
        // alone, with no `{$lua}` marker in front of it, so it reached the
        // assembler as an instruction - "instruction 'return obtainAddresses()'
        // appears before any address or label". That is why RDR2's in-game
        // clock could not be read or set.
        let mut outer = Vec::new();
        let mut enable_body = Vec::new();
        let mut disable_body = Vec::new();
        let mut target: Option<bool> = None;

        for line in script.lines() {
            let trimmed = line.trim();
            if trimmed.eq_ignore_ascii_case("[ENABLE]") {
                target = Some(true);
                continue;
            } else if trimmed.eq_ignore_ascii_case("[DISABLE]") {
                target = Some(false);
                continue;
            }
            match target {
                Some(true) => enable_body.push(line.to_string()),
                Some(false) => disable_body.push(line.to_string()),
                None => outer.push(line.to_string()),
            }
        }

        // A section can only be entered by a marker and never left except by
        // another one, so outer code is always the preamble - prepending it
        // reproduces the in-order rebuild exactly.
        let mut enable_lines = outer.clone();
        enable_lines.extend(enable_body);
        let mut disable_lines = outer;
        disable_lines.extend(disable_body);

        let is_lua = script.contains("{$lua}") || script.contains("{$LUA}");

        Self {
            raw_script: script.to_string(),
            enable_lines,
            disable_lines,
            allocations: HashMap::new(),
            original_bytes: Vec::new(),
            registered: Vec::new(),
            is_lua,
        }
    }

    pub fn execute_enable(
        &mut self,
        process: &mut GameProcess,
        env: &mut ExecEnv<'_>,
        memrec: Option<&RecordView>,
    ) -> Result<()> {
        let lines = self.enable_lines.clone();
        let result = self.execute_section(&lines, process, env, memrec, true);
        if result.is_err() {
            // Roll back so a failed enable does not leak allocations or leave
            // the target half-patched.
            self.rollback(process, env.symbols);
        }
        result
    }

    pub fn execute_disable(
        &mut self,
        process: &mut GameProcess,
        env: &mut ExecEnv<'_>,
        memrec: Option<&RecordView>,
    ) -> Result<()> {
        let lines = self.disable_lines.clone();
        // `false`: do not record originals here. The disable section overwrites
        // the patched bytes, so saving them would make the replay below put the
        // hook straight back.
        let res = self.execute_section(&lines, process, env, memrec, false);

        // Restore anything the disable section did not explicitly rewrite.
        for (addr, bytes) in std::mem::take(&mut self.original_bytes) {
            let _ = process.write_memory(addr, &bytes);
        }

        // **Free nothing here.** A `[DISABLE]` section frees exactly what its
        // own `dealloc()` lines name, and those already ran in
        // `apply_deferred`, which removed them from this map. Whatever is left
        // is memory the table deliberately chose to keep, and it is not ours
        // to reclaim - the conventional behaviour does not reclaim it either.
        //
        // This killed a live game. RDR2's table allocates `WaypointTeleport`,
        // `createthread`s a worker into it, and leaves `[DISABLE]` completely
        // empty. Freeing that allocation unmaps the page a running thread is
        // executing in, and the game dies about a second later - measured, and
        // isolated by disabling the same scripts with that one record left out,
        // which is clean. The hazard was unreachable while `createthread` was
        // broken; making it work made it reachable.
        //
        // It is not only threads. `registersymbol` promotes an allocation's
        // name into the shared table, so a sibling script can still be reading
        // through it after this one is disabled, and `__farptrs` (the
        // far-branch pool, allocated by us rather than the table) is reachable
        // from any injected code that outlives the section.
        //
        // The cost is a genuine leak across enable/disable cycles - a re-enable
        // allocates fresh blocks and these are never reclaimed. The original leaks the
        // same way for the same reason, the blocks are a few KiB, and the
        // target is a 64-bit address space. Crashing the game is not a better
        // trade.
        if !self.allocations.is_empty() {
            let kept: Vec<String> = self
                .allocations
                .drain()
                .map(|(name, (addr, size))| format!("{}@{:#X}({}B)", name, addr, size))
                .collect();
            debug!(
                "disable: leaving {} allocation(s) mapped, as expected - the script's \
                 [DISABLE] did not dealloc them: {}",
                kept.len(),
                kept.join(", ")
            );
        }

        // Only the names this script *shared* need removing. Its allocations
        // were section-local and went with the scope - and unregistering them
        // by name here would delete another script's global symbol that merely
        // happens to be called `newmem`.
        for name in self.registered.drain(..) {
            env.symbols.unregister_symbol(&name);
        }

        res
    }

    /// Runs `{$lua} ... {$asm}` blocks and splices any returned source back in.
    ///
    /// Mirrors the `luacode` pass, and runs before comments are
    /// stripped: Lua bodies use `--` for comments and `//` for integer
    /// division, so the assembler comment rules must not touch them.
    fn run_lua_pass(
        &self,
        raw_lines: &[String],
        process: &mut GameProcess,
        env: &mut ExecEnv<'_>,
        memrec: Option<&RecordView>,
    ) -> Result<Vec<String>> {
        if !raw_lines
            .iter()
            .any(|l| l.trim().eq_ignore_ascii_case("{$lua}"))
        {
            return Ok(raw_lines.to_vec());
        }

        let mut out: Vec<String> = Vec::new();
        let mut i = 0usize;

        while i < raw_lines.len() {
            let t = raw_lines[i].trim();
            if !t.eq_ignore_ascii_case("{$lua}") {
                out.push(raw_lines[i].clone());
                i += 1;
                continue;
            }

            // Collect until {$asm} or end of section.
            let mut body = Vec::new();
            let mut j = i + 1;
            while j < raw_lines.len() && !raw_lines[j].trim().eq_ignore_ascii_case("{$asm}") {
                body.push(raw_lines[j].clone());
                j += 1;
            }

            let code = body.join("\n");
            let mut ctx = LuaCtx {
                process: Some(process),
                symbols: env.symbols,
                records: env.record_views.to_vec(),
                actions: Vec::new(),
            };

            let produced = env
                .lua
                .run_block(&code, memrec, &mut ctx)
                .map_err(|e| anyhow::anyhow!("{{$lua}} block failed: {}", e))?;

            env.actions.append(&mut ctx.actions);

            if let Some(src) = produced {
                for line in src.lines() {
                    out.push(line.to_string());
                }
            }

            // Skip past {$asm} when present.
            i = if j < raw_lines.len() { j + 1 } else { j };
        }

        Ok(out)
    }

    /// Undoes a *failed* `[ENABLE]`, which is the one case where freeing is
    /// still right - see `execute_disable` for why a successful disable must
    /// not.
    ///
    /// The difference is that nothing can be running in these allocations yet.
    /// `createthread` is deferred to `apply_deferred`, which is the last thing
    /// `execute_section` does before returning `Ok`, on both of its exit paths
    /// - so a section that spawned a thread did not fail, and a section that
    /// failed never reached the spawn. `execute_enable` only calls this on
    /// `Err`. Reclaiming here is what keeps the common failure (a table using a
    /// directive this engine does not support) from leaking ~30 blocks per
    /// attempt.
    fn rollback(&mut self, process: &mut GameProcess, symbols: &mut SymbolHandler) {
        for (addr, bytes) in std::mem::take(&mut self.original_bytes) {
            let _ = process.write_memory(addr, &bytes);
        }
        // Their names were section-local, so there is nothing to unregister for
        // them - only for whatever this script promoted with `registersymbol`,
        // below, which other scripts can see and would otherwise be left
        // pointing at freed pages.
        for (_name, (addr, _size)) in self.allocations.drain() {
            let _ = process.free_memory(addr);
        }
        for name in self.registered.drain(..) {
            symbols.unregister_symbol(&name);
        }
    }

    /// Runs one `[ENABLE]` or `[DISABLE]` section inside its own symbol scope.
    ///
    /// The scope is the whole point: `alloc` and `aobscan*` create names that
    /// belong to *this* section, as the format requires, and only
    /// `registersymbol` lifts one into the table every script shares. Without
    /// it, two scripts that both `alloc(newmem,…)` collide, and the second
    /// one's `newmem:` line stops being a label and becomes an address setter
    /// pointing into the first one's live code. See `SymbolHandler::scopes`.
    fn execute_section(
        &mut self,
        raw_lines: &[String],
        process: &mut GameProcess,
        env: &mut ExecEnv<'_>,
        memrec: Option<&RecordView>,
        record_originals: bool,
    ) -> Result<()> {
        env.symbols.push_scope();

        // `[DISABLE]` addresses what `[ENABLE]` allocated - `coord3:` then
        // `db <original bytes>` then `unregistersymbol(coord3)` - so the
        // section starts with those names already in scope.
        let owned: Vec<(String, usize)> = self
            .allocations
            .iter()
            .map(|(name, (addr, _))| (name.clone(), *addr))
            .collect();
        for (name, addr) in owned {
            env.symbols.register_local(&name, addr);
        }

        let result = self.run_section(raw_lines, process, env, memrec, record_originals);
        env.symbols.pop_scope();
        result
    }

    fn run_section(
        &mut self,
        raw_lines: &[String],
        process: &mut GameProcess,
        env: &mut ExecEnv<'_>,
        memrec: Option<&RecordView>,
        record_originals: bool,
    ) -> Result<()> {
        // Lua first, as the format requires: blocks may generate assembler source.
        let raw_lines = self.run_lua_pass(raw_lines, process, env, memrec)?;
        let lines = preprocess(&raw_lines);
        let symbols = &mut *env.symbols;
        if lines.is_empty() {
            return Ok(());
        }

        // ---- Phase 1: directives ------------------------------------------
        // Collected here so the layout pass can start from a fully populated
        // symbol table.
        let mut code_lines: Vec<String> = Vec::new();
        let mut declared_labels: Vec<String> = Vec::new();
        let mut pending_register: Vec<String> = Vec::new();
        // Preferred allocation address, taken from the first alloc that names one.
        let mut alloc_requests: Vec<(String, usize, Option<String>)> = Vec::new();
        // Applied after the section's code has been written; see below.
        let mut deferred_unregister: Vec<String> = Vec::new();
        let mut deferred_dealloc: Vec<String> = Vec::new();
        // Threads start only after the code they run has been written.
        let mut deferred_threads: Vec<String> = Vec::new();

        for line in &lines {
            if let Some(inner) = directive(line, "define") {
                // Handled during preprocessing.
                let _ = inner;
                continue;
            }
            if let Some(inner) = directive(line, "aobscanmodule") {
                let parts = split_args(&inner);
                if parts.len() < 3 {
                    bail!("aobscanmodule expects 3 arguments: {}", line);
                }
                let pattern = parse_aob_pattern(&parts[2])
                    .with_context(|| format!("invalid AOB pattern in: {}", line))?;
                // Two distinct failures, two distinct messages. Sharing one
                // wording made "the module argument did not resolve" and "the
                // pattern is not in the game" indistinguishable in a log, and
                // cost a wasted diagnosis round.
                let module = resolve_scan_module(&parts[1], process, symbols, env.lua)
                    .with_context(|| {
                        format!("aobscanmodule({}): bad module argument", parts[0])
                    })?;
                let addr = aob_scan(process, &pattern, Some(&module))
                    .with_context(|| {
                        format!(
                            "aobscanmodule failed for '{}': pattern not found in {}",
                            parts[0], module
                        )
                    })?;
                symbols.register_local(&parts[0], addr);
                self.registered.push(parts[0].clone());
                tracing::info!("aobscanmodule {} -> {:#X}", parts[0], addr);
                continue;
            }
            if let Some(inner) = directive(line, "aobscan") {
                let parts = split_args(&inner);
                if parts.len() < 2 {
                    bail!("aobscan expects 2 arguments: {}", line);
                }
                let pattern = parse_aob_pattern(&parts[1])
                    .with_context(|| format!("invalid AOB pattern in: {}", line))?;
                let addr = aob_scan(process, &pattern, None)
                    .with_context(|| format!("aobscan failed for '{}'", parts[0]))?;
                symbols.register_local(&parts[0], addr);
                self.registered.push(parts[0].clone());
                continue;
            }
            if let Some(inner) = directive(line, "alloc") {
                let parts = split_args(&inner);
                if parts.is_empty() {
                    bail!("alloc expects at least a name: {}", line);
                }
                let size = parts
                    .get(1)
                    .and_then(|s| parse_number(s))
                    .map(|v| v as usize)
                    .unwrap_or(0x1000);
                alloc_requests.push((parts[0].clone(), size.max(1), parts.get(2).cloned()));
                continue;
            }
            if let Some(inner) = directive(line, "label") {
                for name in split_args(&inner) {
                    declared_labels.push(name);
                }
                continue;
            }
            if let Some(inner) = directive(line, "registersymbol") {
                for name in split_args(&inner) {
                    pending_register.push(name);
                }
                continue;
            }
            if let Some(inner) = directive(line, "unregistersymbol") {
                // Deferred: a [DISABLE] section restores original bytes at
                // `coord3:` and only then unregisters `coord3`. Unregistering
                // during the directive pass would delete the symbol before the
                // code that targets it is laid out.
                for name in split_args(&inner) {
                    deferred_unregister.push(name);
                }
                continue;
            }
            if let Some(inner) = directive(line, "dealloc") {
                // Also deferred: freeing the block before the section's code is
                // emitted would pull the ground out from under it.
                if let Some(name) = split_args(&inner).first() {
                    deferred_dealloc.push(name.clone());
                }
                continue;
            }
            if let Some(inner) = directive(line, "assert") {
                let parts = split_args(&inner);
                if parts.len() == 2 {
                    let addr = symbols
                        .get_address_from_name(&parts[0], Some(process))
                        .with_context(|| format!("assert: unknown symbol '{}'", parts[0]))?;
                    let expected = parse_aob_pattern(&parts[1])?;
                    let mut read = vec![0u8; expected.len()];
                    process.read_memory(addr, &mut read)?;
                    for (j, exp) in expected.iter().enumerate() {
                        if let Some(b) = exp {
                            if read[j] != *b {
                                bail!(
                                    "assert failed at {:#X}: expected {:02X} at +{}, found {:02X}",
                                    addr,
                                    b,
                                    j,
                                    read[j]
                                );
                            }
                        }
                    }
                }
                continue;
            }
            // createthread(x) starts a thread in the target at x. Deferred:
            // the code it executes is written by the emit pass below.
            if let Some(inner) = directive(line, "createthread") {
                if let Some(name) = split_args(&inner).first() {
                    deferred_threads.push(name.clone());
                }
                continue;
            }

            // LuaCall(expr) executes a Lua statement for its side effects and
            // contributes no assembler source.
            if let Some(expr) = directive(line, "luacall") {
                let mut ctx = LuaCtx {
                    process: Some(process),
                    symbols,
                    records: env.record_views.to_vec(),
                    actions: Vec::new(),
                };
                env.lua
                    .run_call(&expr, &mut ctx)
                    .map_err(|e| anyhow::anyhow!("LuaCall({}) failed: {}", expr, e))?;
                env.actions.append(&mut ctx.actions);
                continue;
            }

            // Directives we accept but do not act on.
            if is_ignorable_directive(line) {
                continue;
            }

            code_lines.push(line.clone());
        }

        // ---- Phase 1b: allocations -----------------------------------------
        // A preferred address keeps the trampoline within jmp rel32 range of
        // the hook.
        let far_pool = {
            for (name, size, prefer) in &alloc_requests {
                let preferred = prefer
                    .as_ref()
                    .and_then(|p| symbols.get_address_from_name(p, Some(process)).ok())
                    .unwrap_or(0);

                let addr = if preferred != 0 {
                    process.alloc_memory_near(preferred, *size)?
                } else {
                    // Anchor to another allocation's preferred site when possible so
                    // data blocks stay reachable from the injected code. Failing
                    // that, anchor to the game module: code in an allocation
                    // references other allocations RIP-relatively, and a plain
                    // VirtualAllocEx can land far outside ±2GB of them.
                    let anchor = alloc_requests
                        .iter()
                        .filter_map(|(_, _, p)| p.as_ref())
                        .find_map(|p| symbols.get_address_from_name(p, Some(process)).ok())
                        .unwrap_or_else(|| process.main_module_base());
                    if anchor != 0 {
                        process.alloc_memory_near(anchor, *size)?
                    } else {
                        process.alloc_memory(*size)?
                    }
                };

                // Allocations start zeroed, as tables expect.
                let zeros = vec![0u8; *size];
                let _ = process.write_memory(addr, &zeros);

                symbols.register_local(name, addr);
                self.allocations.insert(name.clone(), (addr, *size));
                debug!("alloc {} = {:#X} ({} bytes)", name, addr, size);
            }

            // A section with no code lays out nothing, so it needs no pool -
            // and must not pay for an allocation it will never use.
            if code_lines.is_empty() {
                None
            } else {
                Some(self.make_far_pool(process, &alloc_requests, symbols))
            }
        };

        let Some(far_pool) = far_pool else {
            // A section with no code still registers its symbols. This used to
            // only book-keep the names and never promote them, so a
            // directives-only script silently lost everything it scanned once
            // the section scope popped.
            //
            // rdr2.CT's BlipPTR is exactly that shape - a nested
            // `autoAssemble([[ aobscanmodule(...) registerSymbol(...) ]])` with
            // no code at all. The scan succeeded, the symbol went into the
            // section scope, the scope was dropped, and the table's next line
            // (`getAddressSafe("BlipPTR")`) got nil.
            promote_registered(
                pending_register,
                &HashMap::new(),
                symbols,
                &mut self.registered,
            );
            apply_deferred(
                &deferred_unregister,
                &deferred_dealloc,
                &deferred_threads,
                &mut self.allocations,
                process,
                symbols,
                &HashMap::new(),
            );
            return Ok(());
        };

        // ---- Phase 2: layout ----------------------------------------------
        let mut label_addrs: HashMap<String, u64> = HashMap::new();
        far_pool.reset();
        let blocks =
            self.layout(&code_lines, process, symbols, &declared_labels, &mut label_addrs, &far_pool)?;
        far_pool.reset();

        // ---- Phase 3: emit -------------------------------------------------
        for block in &blocks {
            let mut addr = block.addr;
            let mut buf: Vec<u8> = Vec::new();

            for (line, expected_len) in &block.lines {
                let bytes =
                    self.assemble_line(line, addr, process, symbols, &label_addrs, &far_pool)?;
                if bytes.len() != *expected_len {
                    bail!(
                        "internal error: '{}' assembled to {} bytes in the emit pass but {} during layout",
                        line.trim(),
                        bytes.len(),
                        expected_len
                    );
                }
                addr += bytes.len() as u64;
                buf.extend_from_slice(&bytes);
            }

            if buf.is_empty() {
                continue;
            }

            // Save what we are about to overwrite, unless this block lives in
            // memory this script allocated (nothing to restore there).
            let start = block.addr as usize;
            if record_originals && !self.is_in_own_allocation(start) {
                let mut original = vec![0u8; buf.len()];
                if process.read_memory(start, &mut original).is_ok() {
                    self.original_bytes.push((start, original));
                }
            }

            process
                .write_memory(start, &buf)
                .with_context(|| format!("failed writing {} bytes at {:#X}", buf.len(), start))?;
            debug!("wrote {} bytes at {:#X}", buf.len(), start);
        }

        far_pool.flush(process);

        // `registersymbol(x)` is the only thing that makes a name visible to
        // other scripts, and it has two kinds of name to promote: a label this
        // section laid out, or one of its own `alloc`/`aobscan` results, which
        // live in the section scope until now.
        promote_registered(
            pending_register,
            &label_addrs,
            symbols,
            &mut self.registered,
        );

        apply_deferred(
            &deferred_unregister,
            &deferred_dealloc,
            &deferred_threads,
            &mut self.allocations,
            process,
            symbols,
            &label_addrs,
        );

        Ok(())
    }

    /// Reserves a small block near the code for far-branch pointers.
    ///
    /// Registered under a private name so `[DISABLE]` frees it with everything
    /// else. Failure is not fatal: only scripts that branch to a Windows API
    /// need it, and they report a clear error if it is missing.
    fn make_far_pool(
        &mut self,
        process: &GameProcess,
        alloc_requests: &[(String, usize, Option<String>)],
        symbols: &SymbolHandler,
    ) -> FarPool {
        const POOL: usize = 256;
        let anchor = alloc_requests
            .iter()
            .filter_map(|(_, _, p)| p.as_ref())
            .find_map(|p| symbols.get_address_from_name(p, Some(process)).ok())
            .unwrap_or_else(|| process.main_module_base());

        match process.alloc_memory_near(anchor, POOL) {
            Ok(base) => {
                self.allocations.insert("__farptrs".to_string(), (base, POOL));
                FarPool { base, size: POOL, ..Default::default() }
            }
            Err(_) => FarPool::default(),
        }
    }

    fn is_in_own_allocation(&self, addr: usize) -> bool {
        self.allocations
            .values()
            .any(|(base, size)| addr >= *base && addr < base + size)
    }

    /// Walks the code assigning an address to every label, without writing.
    fn layout(
        &self,
        code_lines: &[String],
        process: &GameProcess,
        symbols: &SymbolHandler,
        declared_labels: &[String],
        label_addrs: &mut HashMap<String, u64>,
        far_pool: &FarPool,
    ) -> Result<Vec<Block>> {
        // Any `name:` line defines a label. The format requires a matching `label()`
        // declaration; accepting undeclared ones too is strictly more permissive
        // and lets common community tables assemble.
        // A `name:` line is either a *label* (code goes here, address assigned
        // by layout) or an *address setter* (code is written AT that address).
        //
        // The distinction is whether the name already resolves. `_Player:`
        // after `aobscanmodule(_Player,...)` names the injection site in the
        // game; treating it as a fresh label silently redirected the hook into
        // our own allocation and left the game unpatched.
        let resolves = |n: &str| symbols.get_address_from_name(n, Some(process)).is_ok();
        let known = collect_labels(code_lines, declared_labels, &resolves);

        let mut blocks: Vec<Block> = Vec::new();
        let mut cur: Option<Block> = None;
        let mut addr: u64 = 0;
        let mut started = false;

        for line in code_lines {
            if let Some(name) = label_definition(line) {
                let is_label = known.iter().any(|k| k.eq_ignore_ascii_case(&name));

                let new_addr = if is_label {
                    // A label inside an allocated block continues at the current
                    // address; a label naming an allocation jumps to its base.
                    if let Some(&a) = symbols
                        .get_address_from_name(&name, Some(process))
                        .ok()
                        .filter(|_| self.allocations.contains_key(&name))
                        .map(|a| a as u64)
                        .as_ref()
                    {
                        a
                    } else if started {
                        addr
                    } else {
                        bail!("label '{}' is defined before any code block", name);
                    }
                } else if name.contains('[') {
                    // A pointer chain such as `[[base]+40]+80:`. Dereferencing
                    // needs the address parser, not plain symbol arithmetic.
                    crate::address_parser::evaluate_address(&name, Some(process), symbols)
                        .map(|a| a as u64)
                        .with_context(|| format!("cannot resolve code target '{}'", name))?
                } else {
                    // An address expression such as `_Teleport+02:`.
                    resolve_expr(&name, process, symbols, label_addrs)
                        .with_context(|| format!("cannot resolve code target '{}'", name))?
                };

                tracing::info!(
                    "target '{}' -> {:#X} ({})",
                    name,
                    new_addr,
                    if is_label { "label" } else { "address" }
                );
                label_addrs.insert(name.to_lowercase(), new_addr);

                if !started || new_addr != addr {
                    if let Some(b) = cur.take() {
                        blocks.push(b);
                    }
                    cur = Some(Block { addr: new_addr, lines: Vec::new() });
                    addr = new_addr;
                }
                started = true;
                continue;
            }

            if !started {
                bail!("instruction '{}' appears before any address or label", line.trim());
            }

            let len = self.measure_line(line, addr, process, symbols, label_addrs, far_pool)?;
            if let Some(b) = cur.as_mut() {
                b.lines.push((line.clone(), len));
            }
            addr += len as u64;
        }

        if let Some(b) = cur.take() {
            blocks.push(b);
        }
        Ok(blocks)
    }

    /// Length of a line during layout.
    ///
    /// Labels defined later in the script are not known yet, so unresolved
    /// names get a placeholder. Encoded lengths never depend on a symbol's
    /// value - branches are always rel32 and absolute memory operands are
    /// always RIP+disp32 - so the measurement stays correct. The emit pass
    /// resolves strictly and errors on anything still unknown.
    fn measure_line(
        &self,
        line: &str,
        addr: u64,
        process: &GameProcess,
        symbols: &SymbolHandler,
        label_addrs: &HashMap<String, u64>,
        far_pool: &FarPool,
    ) -> Result<usize> {
        Ok(self
            .assemble_line_inner(line, addr, process, symbols, label_addrs, true, far_pool)?
            .len())
    }

    /// Assembles one line, handling data directives before falling back to the
    /// instruction encoder.
    fn assemble_line(
        &self,
        line: &str,
        addr: u64,
        process: &GameProcess,
        symbols: &SymbolHandler,
        label_addrs: &HashMap<String, u64>,
        far_pool: &FarPool,
    ) -> Result<Vec<u8>> {
        self.assemble_line_inner(line, addr, process, symbols, label_addrs, false, far_pool)
    }

    fn assemble_line_inner(
        &self,
        line: &str,
        addr: u64,
        process: &GameProcess,
        symbols: &SymbolHandler,
        label_addrs: &HashMap<String, u64>,
        lenient: bool,
        far_pool: &FarPool,
    ) -> Result<Vec<u8>> {
        let t = line.trim();
        let lower = t.to_lowercase();

        // db/dw/dd/dq data definitions
        for (prefix, width) in [("db ", 1usize), ("dw ", 2), ("dd ", 4), ("dq ", 8)] {
            if let Some(rest) = lower.strip_prefix(prefix) {
                // Slice the *original* line, not the lowercased copy: a string
                // literal keeps its case, and the reference behaviour does the
                // same, re-reading it from the original opcode text.
                let rest = &t[t.len() - rest.len()..];
                let mut out = Vec::new();
                for tok in split_data_operands(rest) {
                    let tok = tok.trim();

                    // A quoted string is characters, not a number.
                    //
                    // `AddString` sets the length to
                    // `length(s)-2` with the comment "not the quotes" and copies
                    // s[2..length-1], so the delimiters are dropped and there is
                    // no NUL terminator. `AddWideString` does the same
                    // in UTF-16.
                    //
                    // Only `db` and `dw` have a string branch;
                    // `dd` and `dq` go straight to HexStrToInt.
                    // Accepting one there would be *more* permissive than the format,
                    // which is its own kind of divergence.
                    if let Some(text) = quoted_literal(tok) {
                        out.extend(string_literal_bytes(text, width).with_context(|| {
                            format!("{} {}", prefix.trim(), tok)
                        })?);
                        continue;
                    }

                    // `db ?` / `??` / `*` / `**` keeps whatever byte is already
                    // there, and `db +N` / `-N` adjusts it. Both are `db`-only -
                    // no other width has the branch - and both need to read the
                    // target, which is why they cannot work offline.
                    //
                    // The address is `addr + out.len()`, i.e. the true offset of
                    // the byte being emitted. The original uses `address + i - 1`, the
                    // *token index*, which silently reads the wrong byte when an
                    // earlier operand was a string. Ours matches for every
                    // input the original gets right, and is correct for the one it is not.
                    if width == 1 {
                        if let Some(op) = byte_patch_op(tok) {
                            let at = addr as usize + out.len();
                            let mut cur = [0u8; 1];
                            process.read_memory(at, &mut cur).with_context(|| {
                                format!(
                                    "'{}' needs the byte already at {:#X}, which could not be read",
                                    tok, at
                                )
                            })?;
                            out.push(match op {
                                BytePatch::Keep => cur[0],
                                BytePatch::AddSigned(d) => cur[0].wrapping_add(d as u8),
                            });
                            continue;
                        }
                    }

                    // Number, then a bare symbol, then a symbol expression.
                    // `dd TEST+3000` is the shape that needs the last one, and
                    // `resolve_expr` already handles `+`/`-` chains - the data
                    // path simply never reached for it.
                    //
                    // Order matters: `parse_number` first, so a plain negative
                    // (`-5`) and the casts keep their meaning rather than being
                    // torn apart as an expression.
                    let v = parse_number(tok)
                        .or_else(|| resolve_name(tok, process, symbols, label_addrs).map(|a| a as i64))
                        .or_else(|| {
                            if tok.contains('+') || tok.contains('-') {
                                resolve_expr(tok, process, symbols, label_addrs)
                                    .ok()
                                    .map(|a| a as i64)
                            } else {
                                None
                            }
                        })
                        .or(if lenient { Some(0) } else { None })
                        .ok_or_else(|| anyhow::anyhow!("invalid data value '{}'", tok))?;
                    out.extend_from_slice(&v.to_le_bytes()[..width]);
                }
                if out.is_empty() {
                    bail!("data directive with no values: {}", t);
                }
                return Ok(out);
            }
        }

        // A data directive with no values contributes nothing. Community
        // tables leave these behind when a block is edited down.
        if matches!(lower.as_str(), "db" | "dw" | "dd" | "dq") {
            return Ok(Vec::new());
        }

        // "nop N" fills N bytes.
        if lower == "nop" {
            return Ok(vec![0x90]);
        }
        if let Some(rest) = lower.strip_prefix("nop ") {
            let n = parse_number(rest.trim()).unwrap_or(1).max(1) as usize;
            return Ok(vec![0x90; n]);
        }

        // "align N[,fill]" pads to an N-byte boundary. The length depends on the
        // current address, which both the layout and emit passes agree on.
        if let Some(rest) = lower.strip_prefix("align ") {
            let args = split_args(rest);
            let boundary = args
                .first()
                .and_then(|s| parse_number(s))
                .filter(|v| *v > 0)
                .unwrap_or(1) as u64;
            let fill = args.get(1).and_then(|s| parse_number(s)).unwrap_or(0x90) as u8;
            let pad = (boundary - (addr % boundary)) % boundary;
            return Ok(vec![fill; pad as usize]);
        }

        // "readmem(source,size)" copies bytes out of the target process.
        if let Some(inner) = directive(t, "readmem") {
            let args = split_args(&inner);
            if args.len() != 2 {
                bail!("readmem expects 2 arguments: {}", t);
            }
            let src = resolve_expr(&args[0], process, symbols, label_addrs)
                .with_context(|| format!("readmem: cannot resolve '{}'", args[0]))?;
            let size = parse_number(&args[1])
                .filter(|v| *v > 0)
                .ok_or_else(|| anyhow::anyhow!("readmem: bad size '{}'", args[1]))?
                as usize;
            let mut buf = vec![0u8; size];
            process
                .read_memory(src as usize, &mut buf)
                .with_context(|| format!("readmem: cannot read {} bytes at {:#X}", size, src))?;
            return Ok(buf);
        }

        let resolver = |name: &str| {
            resolve_name(name, process, symbols, label_addrs).or(if lenient {
                // Placeholder for a label defined further down the script.
                Some(addr)
            } else {
                None
            })
        };
        let far = |target: u64| far_pool.slot(target);
        assemble_with(t, addr, &resolver, &far).with_context(|| format!("assembling '{}'", t))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_name(
    name: &str,
    process: &GameProcess,
    symbols: &SymbolHandler,
    label_addrs: &HashMap<String, u64>,
) -> Option<u64> {
    if let Some(&a) = label_addrs.get(&name.trim().to_lowercase()) {
        return Some(a);
    }
    symbols
        .get_address_from_name(name, Some(process))
        .ok()
        .map(|a| a as u64)
}

fn resolve_expr(
    expr: &str,
    process: &GameProcess,
    symbols: &SymbolHandler,
    label_addrs: &HashMap<String, u64>,
) -> Result<u64> {
    let mut total: i64 = 0;
    let mut sign = 1i64;
    let mut cur = String::new();
    let apply = |tok: &str, sign: i64, total: &mut i64| -> Result<()> {
        let t = tok.trim();
        if t.is_empty() {
            return Ok(());
        }
        let v = parse_number(t)
            .or_else(|| resolve_name(t, process, symbols, label_addrs).map(|a| a as i64))
            .ok_or_else(|| anyhow::anyhow!("cannot resolve '{}'", t))?;
        *total += sign * v;
        Ok(())
    };

    for ch in expr.chars() {
        match ch {
            '+' | '-' => {
                apply(&cur, sign, &mut total)?;
                cur.clear();
                sign = if ch == '-' { -1 } else { 1 };
            }
            _ => cur.push(ch),
        }
    }
    apply(&cur, sign, &mut total)?;
    Ok(total as u64)
}

/// Runs the `unregistersymbol`/`dealloc` directives a section deferred.
fn apply_deferred(
    unregister: &[String],
    dealloc: &[String],
    threads: &[String],
    allocations: &mut HashMap<String, (usize, usize)>,
    process: &mut GameProcess,
    symbols: &mut SymbolHandler,
    label_addrs: &HashMap<String, u64>,
) {
    // Threads first: they run code that the section just wrote, and the
    // symbols they resolve against are about to be unregistered.
    for name in threads {
        match resolve_name(name, process, symbols, label_addrs) {
            Some(addr) => match process.create_thread(addr as usize) {
                Ok(tid) => info!("createthread({}) -> tid {} at {:#X}", name, tid, addr),
                Err(e) => warn!("createthread({}) failed: {:#}", name, e),
            },
            None => warn!("createthread({}): unknown symbol", name),
        }
    }

    for name in dealloc {
        if let Some((addr, _)) = allocations.remove(name.as_str()) {
            let _ = process.free_memory(addr);
        }
        symbols.unregister_symbol(name);
    }
    for name in unregister {
        symbols.unregister_symbol(name);
    }
}

/// Decides which `name:` lines are labels rather than address setters.
///
/// A `name:` line is either a **label** (code goes here; layout assigns the
/// address) or an **address setter** (code is written AT that address). The
/// distinction is whether the name already resolves: `_Player:` following
/// `aobscanmodule(_Player,...)` names an injection site in the game, and
/// treating it as a fresh label silently redirects the hook into the
/// trainer's own allocation, leaving the game unpatched.
fn collect_labels(
    code_lines: &[String],
    declared_labels: &[String],
    resolves: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut known: Vec<String> = declared_labels.to_vec();
    for line in code_lines {
        let Some(name) = label_definition(line) else { continue };
        if name.contains('+') || name.contains('-') || name.contains('[') {
            continue;
        }
        if parse_number(&name).is_none() && !resolves(&name) {
            known.push(name);
        }
    }
    known
}

/// Returns the name from a `something:` line.
fn label_definition(line: &str) -> Option<String> {
    let t = line.trim();
    if !t.ends_with(':') {
        return None;
    }
    let name = t[..t.len() - 1].trim();
    if name.is_empty() {
        return None;
    }
    // Module names may contain spaces, e.g. `"1001 Spikes.exe"+349CD:`.
    // Only a space outside quotes means this is an instruction, not a target.
    let mut in_quotes = false;
    for ch in name.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ' ' | '\t' if !in_quotes => return None,
            _ => {}
        }
    }
    // `[[base]+40]+80:` is a valid code target: a pointer chain resolved at
    // injection time. Bracketed forms are targets, never label names.
    Some(name.to_string())
}

/// Matches `name(...)` and returns the argument text.
/// Applies `registersymbol(x)` for a finished section.
///
/// Shared by both exits from `execute_section` - the normal one and the
/// early return taken when a section lays out no code - because having two
/// copies is what let the second one quietly skip promotion entirely.
///
/// Two kinds of name reach here: a label this section laid out, and one of its
/// own `alloc`/`aobscan` results, which live in the section scope until
/// promoted. `registersymbol` is the only thing that makes either visible to
/// other scripts, so a miss here is invisible until some
/// later record addressed by the name reads `??`.
fn promote_registered(
    pending: Vec<String>,
    label_addrs: &HashMap<String, u64>,
    symbols: &mut SymbolHandler,
    registered: &mut Vec<String>,
) {
    for name in pending {
        if let Some(&addr) = label_addrs.get(&name.to_lowercase()) {
            symbols.register_symbol(&name, addr as usize);
        } else if !symbols.promote(&name) {
            // There would be nothing to register either; say so rather than
            // leaving the table to fail somewhere later for no visible reason.
            warn!(
                "registersymbol({}): nothing by that name was allocated, scanned or labelled in this script",
                name
            );
        }
        registered.push(name);
    }
}

/// Splits a `db`/`dw`/`dd`/`dq` operand list, keeping quoted strings whole.
///
/// Separators are comma and whitespace, as before - but never *inside* a
/// `'...'` literal. Splitting on space unconditionally is what used to tear
/// `db 'Hello World'` into `'Hello` and `World'`, and reject
/// `db '=========>'` outright. Decorative separator strings like that are the
/// single most common thing this engine could not assemble across a
/// 4,350-table corpus.
///
/// An unterminated quote yields the rest of the line as one token, which then
/// fails to parse and is reported with the line in the message - better than
/// silently dropping the tail.
pub fn split_data_operands(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;

    for c in s.chars() {
        match c {
            '\'' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            ',' if !in_quotes => {
                if !cur.trim().is_empty() {
                    out.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if !cur.trim().is_empty() {
                    out.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// What a `db` operand does to the byte already in the target.
enum BytePatch {
    /// `?` / `??` / `*` / `**` - re-emit it unchanged.
    Keep,
    /// `+N` / `-N` - add to it. N is parsed with `HexStrToInt`, so `+10` is
    /// `+0x10`, not ten.
    AddSigned(i64),
}

/// Recognises the two in-place `db` operands.
///
/// The wildcard test is deliberately narrow, matching the format exactly: the first
/// character must be `?` or `*`, and the token must be either one character or
/// two *identical* ones. `?x` and `?*` are not wildcards and fall through to the
/// normal parse rather than silently preserving a byte.
fn byte_patch_op(tok: &str) -> Option<BytePatch> {
    let b = tok.as_bytes();
    if b.is_empty() {
        return None;
    }
    if (b[0] == b'?' || b[0] == b'*') && (b.len() == 1 || (b.len() == 2 && b[1] == b[0])) {
        return Some(BytePatch::Keep);
    }
    if b[0] == b'+' || b[0] == b'-' {
        // Hex, per HexStrToInt - and a bare sign is not an operand.
        let magnitude = i64::from_str_radix(&tok[1..], 16).ok()?;
        return Some(BytePatch::AddSigned(if b[0] == b'-' { -magnitude } else { magnitude }));
    }
    None
}

/// Whether a `db` operand is one of the in-place forms. For the offline
/// analyser, which cannot evaluate them but must not report them as failures.
pub fn is_byte_patch_operand(tok: &str) -> bool {
    byte_patch_op(tok).is_some()
}

/// The bytes a `'...'` literal contributes to a data directive.
///
/// `db` is one byte per character (`AddString`) and
/// `dw` is UTF-16 (`AddWideString`). Neither emits the quotes - the assembler
/// sizes the copy `length(s)-2` with the comment "not the quotes" - and neither
/// NUL-terminates.
///
/// `dd` and `dq` have no string branch at all, so a literal
/// there is an error. Accepting one would be more permissive than the format, and would
/// silently produce a different byte count than the table author saw.
fn string_literal_bytes(text: &str, width: usize) -> Result<Vec<u8>> {
    match width {
        1 => Ok(text.bytes().collect()),
        2 => Ok(text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()),
        _ => bail!("string literals are only valid in db and dw, not a {}-byte directive", width),
    }
}

/// The contents of a `'...'` literal, or `None` if the token is not one.
///
/// A lone `''` is an empty string and contributes no bytes, which is what the
/// `length(s)-2` arithmetic produces too.
pub fn quoted_literal(tok: &str) -> Option<&str> {
    let b = tok.as_bytes();
    if b.len() >= 2 && b[0] == b'\'' && b[b.len() - 1] == b'\'' {
        Some(&tok[1..tok.len() - 1])
    } else {
        None
    }
}

fn directive(line: &str, name: &str) -> Option<String> {
    let t = line.trim();
    let lower = t.to_lowercase();
    let prefix = format!("{}(", name);
    if !lower.starts_with(&prefix) || !t.ends_with(')') {
        return None;
    }
    Some(t[prefix.len()..t.len() - 1].to_string())
}

fn is_ignorable_directive(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    for d in [
        "loadlibrary(",
        "globalalloc(",
        "fullaccess(",
        "aobscanregion(",
        "include(",
        "usemono(",
        "monoaobscan(",
        "loadbinary(",
        "savebinary(",
        "unlockmemory(",
        "sharedalloc(",
        "struct ",
        "endstruct",
        "searchedaob",
    ] {
        if lower.starts_with(d) {
            return true;
        }
    }
    // Compiler directives, e.g. {$try} / {$except} / {$strict}.
    if lower.starts_with("{$") && lower.ends_with('}') {
        return true;
    }
    matches!(lower.as_str(), "[enable]" | "[disable]")
}

/// Splits directive arguments on commas at paren/bracket depth zero.
fn split_args(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in inner.chars() {
        match ch {
            '(' | '[' => {
                depth += 1;
                cur.push(ch);
            }
            ')' | ']' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out.retain(|s| !s.is_empty());
    out
}

/// Extracts the `{$lua} ... {$asm}` bodies from a section, for offline checks.
pub fn extract_lua_blocks(raw_lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < raw_lines.len() {
        if !raw_lines[i].trim().eq_ignore_ascii_case("{$lua}") {
            i += 1;
            continue;
        }
        let mut body = Vec::new();
        let mut j = i + 1;
        while j < raw_lines.len() && !raw_lines[j].trim().eq_ignore_ascii_case("{$asm}") {
            body.push(raw_lines[j].clone());
            j += 1;
        }
        out.push(body.join("\n"));
        i = if j < raw_lines.len() { j + 1 } else { j };
    }
    out
}

/// Extracts `LuaCall(...)` argument text from a section.
pub fn extract_lua_calls(raw_lines: &[String]) -> Vec<String> {
    preprocess(raw_lines)
        .iter()
        .filter_map(|l| directive(l, "luacall"))
        .collect()
}

/// Preprocessing exposed for the `--analyze` coverage report.
pub fn preprocess_for_analysis(raw_lines: &[String]) -> Vec<String> {
    preprocess(raw_lines)
}

/// True for lines the engine handles outside the instruction encoder:
/// directives, label definitions and data definitions.
pub fn is_non_code_line(line: &str) -> bool {
    if label_definition(line).is_some() || is_ignorable_directive(line) {
        return true;
    }
    for d in [
        "aobscanmodule",
        "aobscan",
        "alloc",
        "dealloc",
        "label",
        "registersymbol",
        "unregistersymbol",
        "assert",
        "define",
        "readmem",
        "luacall",
    ] {
        if directive(line, d).is_some() {
            return true;
        }
    }
    let lower = line.trim().to_lowercase();
    matches!(lower.as_str(), "db" | "dw" | "dd" | "dq" | "nop")
        || lower.starts_with("db ")
        || lower.starts_with("dw ")
        || lower.starts_with("dd ")
        || lower.starts_with("dq ")
        || lower.starts_with("nop ")
        || lower.starts_with("align ")
}

/// Strips comments and `{...}` blocks, and expands `define()` macros.
fn preprocess(raw_lines: &[String]) -> Vec<String> {
    let mut defines: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    let mut brace_depth = 0i32;
    let mut in_block_comment = false;

    for raw in raw_lines {
        // Normalise tabs so `db\t90` is recognised like `db 90`.
        let mut line = raw.replace('\t', " ");

        // C-style /* ... */ block comments.
        if in_block_comment {
            match line.find("*/") {
                Some(pos) => {
                    line = line[pos + 2..].to_string();
                    in_block_comment = false;
                }
                None => continue,
            }
        }
        loop {
            let Some(start) = line.find("/*") else { break };
            match line[start..].find("*/") {
                Some(rel) => {
                    let end = start + rel + 2;
                    line = format!("{}{}", &line[..start], &line[end..]);
                }
                None => {
                    line = line[..start].to_string();
                    in_block_comment = true;
                    break;
                }
            }
        }

        // Multi-line {...} comment blocks. Tables use these to stash the original
        // disassembly, which must never be assembled.
        if brace_depth > 0 {
            brace_depth += line.matches('{').count() as i32;
            brace_depth -= line.matches('}').count() as i32;
            continue;
        }
        if let Some(pos) = line.find('{') {
            // Directives like {$lua} / {$asm} are not comments.
            let rest = &line[pos..];
            if !rest.starts_with("{$") {
                let opens = line.matches('{').count() as i32;
                let closes = line.matches('}').count() as i32;
                line = line[..pos].to_string();
                if opens > closes {
                    brace_depth = opens - closes;
                }
            }
        }

        // Line comments
        if let Some(pos) = line.find("//") {
            line = line[..pos].to_string();
        }

        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        // Tables use punctuation runs as visual separators (`/`, `---`, `***`).
        // Every real instruction, label and directive contains a letter or digit.
        if !trimmed.chars().any(|c| c.is_alphanumeric()) {
            continue;
        }

        if let Some(inner) = directive(&trimmed, "define") {
            let parts = split_args(&inner);
            if parts.len() == 2 {
                defines.push((parts[0].clone(), parts[1].clone()));
            }
            continue;
        }

        let mut expanded = trimmed;
        for (k, v) in &defines {
            expanded = replace_token(&expanded, k, v);
        }
        out.push(expanded);
    }

    out
}

/// Replaces whole-token occurrences only, so a define named `x` does not
/// corrupt `xmm0` or `maxhp`.
fn replace_token(haystack: &str, token: &str, replacement: &str) -> String {
    if token.is_empty() {
        return haystack.to_string();
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(haystack.len());
    let bytes: Vec<char> = haystack.chars().collect();
    let tok: Vec<char> = token.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let matches = i + tok.len() <= bytes.len()
            && bytes[i..i + tok.len()]
                .iter()
                .zip(tok.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b));
        let left_ok = i == 0 || !is_word(bytes[i - 1]);
        let right_ok = i + tok.len() >= bytes.len() || !is_word(bytes[i + tok.len()]);
        if matches && left_ok && right_ok {
            out.push_str(replacement);
            i += tok.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_enable_and_disable_sections() {
        let s = AutoAssemblerScript::parse(
            "[ENABLE]\nalloc(x,100)\nx:\ndb 90\n[DISABLE]\ndealloc(x)\n",
        );
        assert_eq!(s.enable_lines.len(), 3);
        assert_eq!(s.disable_lines.len(), 1);
    }

    #[test]
    fn preprocess_drops_original_code_blocks() {
        let lines: Vec<String> = "aobscanmodule(a,g.exe,90 90)\n\
             {\n\
             // ORIGINAL CODE\n\
             g.exe+123: 48 8B CE - mov rcx,rsi\n\
             }\n\
             mov rax,1"
            .lines()
            .map(|s| s.to_string())
            .collect();
        let out = preprocess(&lines);
        assert_eq!(out, vec!["aobscanmodule(a,g.exe,90 90)", "mov rax,1"]);
    }

    #[test]
    fn preprocess_keeps_dollar_directives() {
        let lines: Vec<String> = vec!["{$lua}".into(), "print(1)".into()];
        let out = preprocess(&lines);
        assert_eq!(out[0], "{$lua}");
    }

    #[test]
    fn alloc_parses_three_argument_form() {
        // The size must not absorb the preferred-address argument.
        let inner = directive("alloc(newmem,$1000,\"RDR2.exe\"+66AC6F)", "alloc").unwrap();
        let parts = split_args(&inner);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "newmem");
        assert_eq!(parse_number(&parts[1]), Some(0x1000));
        assert_eq!(parts[2], "\"RDR2.exe\"+66AC6F");
    }

    /// The bug that left RDR2 unpatched: an AOB-resolved name used as a code
    /// target was treated as a new label, so the hook was written into the
    /// trainer's own allocation instead of the game.
    #[test]
    fn aob_resolved_names_are_address_setters_not_labels() {
        let lines: Vec<String> = vec![
            "newmemPlayer:".into(),
            "mov [_pPlayer],rbx".into(),
            "jmp returnPlayer".into(),
            "_Player:".into(),
            "jmp newmemPlayer".into(),
            "nop".into(),
            "returnPlayer:".into(),
        ];
        // _Player and newmemPlayer already exist (aobscan result / alloc);
        // returnPlayer is a plain label defined by this script.
        let resolves = |n: &str| matches!(n, "_Player" | "newmemPlayer");

        let known = collect_labels(&lines, &[], &resolves);

        assert!(!known.iter().any(|k| k == "_Player"), "_Player must stay an address");
        assert!(!known.iter().any(|k| k == "newmemPlayer"), "alloc names are addresses");
        assert!(known.iter().any(|k| k == "returnPlayer"), "undeclared labels still work");
    }

    #[test]
    fn declared_labels_win_even_if_they_would_resolve() {
        let lines: Vec<String> = vec!["code:".into(), "nop".into()];
        // Declared via label(code); must be a label regardless.
        let known = collect_labels(&lines, &["code".to_string()], &|_| true);
        assert!(known.iter().any(|k| k == "code"));
    }

    #[test]
    fn label_definitions_are_recognised() {
        assert_eq!(label_definition("newmem:"), Some("newmem".into()));
        assert_eq!(label_definition("  _Teleport+02:"), Some("_Teleport+02".into()));
        assert_eq!(label_definition("mov rax,1"), None);
        assert_eq!(label_definition("mov [rax],1"), None);
        // Module names can contain spaces; the quotes make it a target, not code.
        assert_eq!(
            label_definition("\"1001 Spikes.exe\"+349CD:"),
            Some("\"1001 Spikes.exe\"+349CD".into())
        );
        // Bare data directives contribute nothing but must not reach the encoder.
        assert!(is_non_code_line("db"));
        assert!(is_non_code_line("dq"));
    }

    #[test]
    fn define_expansion_is_token_aware() {
        assert_eq!(replace_token("mov xmm0,x", "x", "5"), "mov xmm0,5");
        assert_eq!(replace_token("mov maxhp,1", "hp", "2"), "mov maxhp,1");
    }

    #[test]
    fn multi_name_label_and_registersymbol() {
        assert_eq!(split_args("code, return"), vec!["code", "return"]);
    }

    /// The `[ENABLE]` body of "Single Player Teleport" from the Red Dead
    /// Redemption 2 table. This is the script that populates `_pPos`, which the
    /// Coord X/Y/Z records address as `_pPos` + offset 70/74/78.
    const RDR2_TELEPORT_ENABLE: &str = r#"
aobscanmodule(_Teleport,RDR2.exe,EB 23 F3 41 0F 6F 47 70) // should be unique
alloc(newmem,$1000,"RDR2.exe"+66AC6F)
alloc(_xPos,8)
alloc(_pPos,8)
registersymbol(_pPos)
registersymbol(_Teleport)
label(code)
label(return)

newmem:
cmp r14,[_pPlayer]
jne code
  movdqu xmm0,[r15+70]
mov [_pPos],r15
cmp [_savePos],1
je Save
jmp code

Save:
mov [_savePos],0
push rax
mov rax,[r15+70]
mov [_xPos],rax
pop rax
jmp code

code:
  movdqu xmm0,[r15+70]
  jmp return

_Teleport+02:
  jmp newmem
  nop
return:

_xPos:
dd 0
"#;

    /// Every instruction in the real script must encode. Before this work the
    /// engine silently skipped all of them, so the hook was never installed and
    /// `_pPos` stayed zeroed - which is why the coordinates read wrong.
    #[test]
    fn rdr2_teleport_script_fully_assembles() {
        use crate::assembler::assemble;

        let raw: Vec<String> = RDR2_TELEPORT_ENABLE.lines().map(|s| s.to_string()).collect();
        let lines = preprocess(&raw);

        // Stand-in addresses for what the directives would resolve at runtime.
        let syms: Vec<(&str, u64)> = vec![
            ("_teleport", 0x7FF6_0066_AC6F),
            ("newmem", 0x7FF6_0100_0000),
            ("_xpos", 0x7FF6_0100_1000),
            ("_ppos", 0x7FF6_0100_1010),
            ("_pplayer", 0x7FF6_0100_1020),
            ("_savepos", 0x7FF6_0100_1030),
            ("code", 0x7FF6_0100_0100),
            ("return", 0x7FF6_0066_AC75),
            ("save", 0x7FF6_0100_0080),
        ];
        let resolve = |n: &str| {
            syms.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n.trim()))
                .map(|(_, v)| *v)
        };

        let mut encoded = 0usize;
        let mut total_bytes = 0usize;
        let mut addr = 0x7FF6_0100_0000u64;

        for line in &lines {
            if label_definition(line).is_some() || is_ignorable_directive(line) {
                continue;
            }
            if ["aobscanmodule", "alloc", "registersymbol", "label"]
                .iter()
                .any(|d| directive(line, d).is_some())
            {
                continue;
            }
            // Data directives are handled by assemble_line, not the encoder.
            let lower = line.to_lowercase();
            if lower.starts_with("dd ") || lower.starts_with("db ") {
                continue;
            }
            if lower == "nop" {
                encoded += 1;
                total_bytes += 1;
                addr += 1;
                continue;
            }

            let bytes = assemble(line, addr, &resolve)
                .unwrap_or_else(|e| panic!("failed to assemble '{}': {}", line.trim(), e));
            assert!(!bytes.is_empty(), "'{}' produced no bytes", line.trim());
            encoded += 1;
            total_bytes += bytes.len();
            addr += bytes.len() as u64;
        }

        // 15 instructions in the newmem/Save/code blocks, plus the `jmp newmem`
        // and `nop` that form the hook at the injection site.
        assert_eq!(encoded, 17, "expected every instruction line to encode");
        assert!(total_bytes > 40, "suspiciously little code emitted");
    }

    /// The whole point of the `{$lua}` contract: a block returns assembler
    /// source, which the encoder then has to accept.
    #[test]
    fn lua_generated_source_feeds_the_assembler() {
        use crate::assembler::assemble;
        use crate::lua_engine::{LuaCtx, LuaEngine};
        use crate::symbol_handler::SymbolHandler;

        let engine = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };

        // A table generating a run of writes - the common codegen pattern.
        let produced = engine
            .run_block(
                "local out = {}
                 for i = 0, 3 do
                   out[#out+1] = string.format('mov [rbx+%X],rax', 0x10 + i * 8)
                 end
                 return table.concat(out, '\\n')",
                None,
                &mut ctx,
            )
            .unwrap()
            .expect("block should produce source");

        let lines: Vec<&str> = produced.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "mov [rbx+10],rax");
        assert_eq!(lines[3], "mov [rbx+28],rax");

        // Every generated line must assemble.
        let none = |_: &str| None;
        let mut addr = 0x7FF6_0100_0000u64;
        for line in &lines {
            let bytes = assemble(line, addr, &none)
                .unwrap_or_else(|e| panic!("lua-generated '{}' failed: {}", line, e));
            assert!(!bytes.is_empty());
            addr += bytes.len() as u64;
        }
    }

    /// `{$lua}` bodies must not be mangled by assembler comment rules: Lua uses
    /// `--` for comments and `//` for integer division.
    #[test]
    fn lua_bodies_survive_the_comment_stripper() {
        use crate::lua_engine::{LuaCtx, LuaEngine};
        use crate::symbol_handler::SymbolHandler;

        let engine = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };

        let out = engine
            .run_block(
                "-- a Lua comment containing // and { braces }\n\
                 local half = 9 // 2\n\
                 return tostring(half)",
                None,
                &mut ctx,
            )
            .unwrap();
        assert_eq!(out.as_deref(), Some("4"));
    }

    /// The hook itself must be exactly 5 bytes of `jmp rel32` plus one `nop`,
    /// so it overwrites the 6-byte `movdqu xmm0,[r15+70]` cleanly.
    #[test]
    fn injection_jmp_matches_the_overwritten_instruction_length() {
        use crate::assembler::assemble;
        let hook_site = 0x7FF6_0066_AC71u64;
        let newmem = 0x7FF6_0100_0000u64;
        let resolve = |n: &str| (n.trim().eq_ignore_ascii_case("newmem")).then_some(newmem);

        let jmp = assemble("jmp newmem", hook_site, &resolve).unwrap();
        assert_eq!(jmp.len(), 5);
        assert_eq!(jmp[0], 0xE9);
        // jmp + nop == the 6 bytes of the original instruction.
        assert_eq!(jmp.len() + 1, 6);
    }

    /// The code *outside* both sections is kept and prepended to whichever
    /// one runs, leaving "only the outer code and the selected script
    /// routine".
    ///
    /// Dropping it broke every table using the Otis_Inf / SunBeam shape - a
    /// `{$lua}` preamble of helper functions, then a section body that is just
    /// `return someFunction()`. Without the preamble the `return` has no
    /// `{$lua}` in front of it and reaches the assembler as an instruction,
    /// which is what stopped RDR2's in-game clock from being readable.
    #[test]
    fn code_outside_both_sections_belongs_to_each_of_them() {
        let script = "\
{$STRICT}
{$lua}
function obtainAddresses()
  registerSymbol(\"g_todHours\", 0x1000, true)
end
[ENABLE]
return obtainAddresses()
[DISABLE]
return releaseAddresses()";

        let s = AutoAssemblerScript::parse(script);

        // The preamble reaches both sections...
        for (name, lines) in [("enable", &s.enable_lines), ("disable", &s.disable_lines)] {
            assert!(
                lines.iter().any(|l| l.trim() == "{$lua}"),
                "{name}: lost the {{$lua}} marker, so the body would be assembled"
            );
            assert!(
                lines.iter().any(|l| l.contains("function obtainAddresses")),
                "{name}: lost the helper the body calls"
            );
        }

        // ...and each keeps only its own body.
        assert!(s.enable_lines.iter().any(|l| l.trim() == "return obtainAddresses()"));
        assert!(!s.enable_lines.iter().any(|l| l.contains("releaseAddresses")));
        assert!(s.disable_lines.iter().any(|l| l.trim() == "return releaseAddresses()"));
        // Note the body line, not the substring: the *preamble* legitimately
        // contains `function obtainAddresses()` in both sections.
        assert!(!s
            .disable_lines
            .iter()
            .any(|l| l.trim() == "return obtainAddresses()"));

        // The markers themselves are stripped, via `continue`.
        assert!(!s.enable_lines.iter().any(|l| l.trim().eq_ignore_ascii_case("[ENABLE]")));
    }

    /// `db`/`dw` string literals, as the assembler handles them.
    ///
    /// `AddString` sizes the output `length(s)-2` with the comment
    /// "not the quotes" and copies `s[2..length-1]`, so the delimiters are
    /// dropped and nothing is NUL-terminated. `AddWideString` is the
    /// same in UTF-16. `dd` and `dq` have no string branch
    /// at all, so a literal there must be an error rather than quietly accepted.
    #[test]
    fn data_string_literals_follow_ce() {
        // Tokenizer: quotes survive separators.
        assert_eq!(split_data_operands("'A B'"), vec!["'A B'"]);
        assert_eq!(split_data_operands("'A,B'"), vec!["'A,B'"]);
        assert_eq!(split_data_operands("41, 42"), vec!["41", "42"]);
        assert_eq!(split_data_operands("'AB',00"), vec!["'AB'", "00"]);
        // The decorative separators that dominate the corpus.
        assert_eq!(split_data_operands("'=====>'"), vec!["'=====>'"]);

        assert_eq!(quoted_literal("'AB'"), Some("AB"));
        assert_eq!(quoted_literal("''"), Some(""));
        assert_eq!(quoted_literal("41"), None);
        assert_eq!(quoted_literal("'unterminated"), None);
    }

    #[test]
    fn db_emits_characters_and_dw_emits_utf16() {
        // db: one byte per character, quotes dropped, no NUL.
        assert_eq!(string_literal_bytes("AB", 1).unwrap(), vec![0x41, 0x42]);
        // A space inside the literal is content, not a separator.
        assert_eq!(string_literal_bytes("A B", 1).unwrap(), vec![0x41, 0x20, 0x42]);
        // The decorative separators that dominate the corpus.
        assert_eq!(string_literal_bytes("=>", 1).unwrap(), vec![0x3D, 0x3E]);
        // An empty literal contributes nothing, as the length-2 arithmetic does.
        assert_eq!(string_literal_bytes("", 1).unwrap(), Vec::<u8>::new());

        // dw: UTF-16LE.
        assert_eq!(string_literal_bytes("AB", 2).unwrap(), vec![0x41, 0x00, 0x42, 0x00]);

        // dd/dq have no string branch - refuse rather than invent one.
        assert!(string_literal_bytes("AB", 4).is_err());
        assert!(string_literal_bytes("AB", 8).is_err());
    }


    /// the in-place `db` operands.
    ///
    /// The wildcard test is narrow on purpose: first char `?` or `*`, and
    /// either one character or two *identical* ones. Widening it would make
    /// `?x` silently preserve a byte instead of failing to parse, which is the
    /// quiet-wrong-bytes class this codebase treats as the cardinal sin.
    #[test]
    fn in_place_db_operands() {
        assert!(matches!(byte_patch_op("?"), Some(BytePatch::Keep)));
        assert!(matches!(byte_patch_op("??"), Some(BytePatch::Keep)));
        assert!(matches!(byte_patch_op("*"), Some(BytePatch::Keep)));
        assert!(matches!(byte_patch_op("**"), Some(BytePatch::Keep)));

        // Not wildcards - the pair is required to match.
        assert!(!matches!(byte_patch_op("?x"), Some(BytePatch::Keep)));
        assert!(!matches!(byte_patch_op("?*"), Some(BytePatch::Keep)));
        assert!(byte_patch_op("???").is_none());

        // Relative bytes are HEX, per HexStrToInt: `+10` is +0x10, not ten.
        assert!(matches!(byte_patch_op("+10"), Some(BytePatch::AddSigned(0x10))));
        assert!(matches!(byte_patch_op("-1"), Some(BytePatch::AddSigned(-1))));
        assert!(matches!(byte_patch_op("+FF"), Some(BytePatch::AddSigned(0xFF))));

        // A bare sign is not an operand.
        assert!(byte_patch_op("+").is_none());
        assert!(byte_patch_op("-").is_none());

        // Ordinary values are untouched by this path.
        assert!(byte_patch_op("90").is_none());
        assert!(byte_patch_op("'AB'").is_none());
    }

}

/// Resolves `aobscanmodule`'s module argument the way the format expects.
///
/// The name is tried as a module first and, failing that, resolved as a
/// symbol and uses whichever module *contains* the resulting address
/// - its diagnostic there is "modulename not found. Perhaps a symbol was
/// used". A leading `$` means "evaluate as Lua",
/// and a string result is then resolved in its own right.
///
/// Those two rules together are what make the common idiom work:
///
/// ```text
/// aobscanmodule(BlipPTR,$process,48 8b 3d ? ? ? ? ...)
/// ```
///
/// `$process` reads the Lua global `process`, which holds the target's module
/// name, so the scan is bounded to the main
/// module without the table hardcoding its filename. Without this the argument
/// was passed through literally, no module called `$process` existed, and the
/// scan failed - which is why rdr2.CT's BlipPTR never enabled.
///
/// Failure is an error rather than a whole-address-space scan. The original raises
/// `rsAAModuleNotFound`, and silently widening the range would turn a specific
/// signature into one that can match anywhere and then get hooked.
fn resolve_scan_module(
    raw: &str,
    process: &GameProcess,
    symbols: &SymbolHandler,
    lua: &crate::lua_engine::LuaEngine,
) -> Result<String> {
    fn as_module(name: &str, process: &GameProcess) -> Option<String> {
        let key = name.trim().replace('"', "").trim().to_lowercase();
        process.modules.contains_key(&key).then_some(key)
    }

    let name = raw.trim().replace('"', "");
    let name = name.trim();

    if let Some(m) = as_module(name, process) {
        return Ok(m);
    }

    // `$x` - evaluate as Lua, then resolve whatever it produced.
    if let Some(rest) = name.strip_prefix('$') {
        let looked_up = lua.global_string(rest);
        info!(
            "aobscanmodule module arg '{}': lua global '{}' = {:?}",
            name, rest, looked_up
        );
        if let Some(value) = looked_up {
            if let Some(m) = as_module(&value, process) {
                return Ok(m);
            }
            // A Lua string that is not itself a module still gets the symbol
            // treatment below, via its address.
            if let Ok(addr) = symbols.get_address_from_name(&value, Some(process)) {
                if let Some(m) = module_containing(addr, process) {
                    return Ok(m);
                }
            }
        }
    }

    // A symbol, then the module it lives in.
    if let Ok(addr) = symbols.get_address_from_name(name, Some(process)) {
        if let Some(m) = module_containing(addr, process) {
            return Ok(m);
        }
    }

    bail!(
        "module '{}' not found, and it does not resolve to an address inside one",
        raw.trim()
    )
}

fn module_containing(addr: usize, process: &GameProcess) -> Option<String> {
    process
        .modules
        .iter()
        .find(|(_, m)| addr >= m.base && addr < m.base + m.size)
        .map(|(key, _)| key.clone())
}
