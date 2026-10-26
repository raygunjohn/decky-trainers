//! Table Lua API, headless.
//!
//! Tables lean heavily on Lua: across the bundled corpus, 12087 of 33129
//! Auto Assembler scripts have a `{$lua}` body rather than assembler. Without
//! this, those features cannot work no matter how good the assembler is.
//!
//! Semantics follow the `luacode` pass:
//!
//! - A `{$lua} ... {$asm}` block is compiled as a chunk whose first line is
//!   `local syntaxcheck,memrec=...`, then called with those two arguments.
//! - The block's source lines are removed from the script.
//! - **If the chunk returns a string, that string is spliced back in as
//!   assembler source.** This is how tables generate code at inject time.
//!
//! `LuaCall(expr)` executes `expr` as a statement and contributes no source.
//!
//! # Scope of this implementation
//!
//! Lua runs synchronously against the live process and symbol table. Access
//! goes through a [`Bridge`] that is **invalidated when the call returns**, so
//! a script that stashes an API function in a global and calls it later gets a
//! clean error instead of touching freed state.
//!
//! Two consequences, both deliberate:
//!
//! - Memory-record activation from Lua is **queued** and applied after the
//!   block finishes, rather than recursing back into the AA engine mid-script.
//! - `createThread` does not run its body. A trainer thread is typically an
//!   endless `while not terminated` loop, which would hang a synchronous call.
//!   It logs a warning so the feature is visibly unsupported rather than
//!   silently inert.

use crate::address_parser::evaluate_address;
use crate::aob_scanner::{aob_scan, parse_aob_pattern};
use crate::process::GameProcess;
use crate::symbol_handler::SymbolHandler;
use mlua::{Lua, MultiValue, Table, Value, Variadic};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// A record as Lua sees it. Snapshotted before the block runs so Lua never
/// needs access to the live record tree.
#[derive(Debug, Clone)]
pub struct RecordView {
    pub id: u32,
    pub description: String,
    pub address_expr: String,
    pub active: bool,
    pub is_aa: bool,
    /// IDs of direct children, for the `memrec.Child[i]` / `memrec.Count`.
    pub child_ids: Vec<u32>,
}

/// Work Lua asked for that must be applied after it returns.
#[derive(Debug, Clone, PartialEq)]
pub enum LuaAction {
    SetActive { id: u32, active: bool },
    SetValue { id: u32, value: String },
    /// The table named a process to attach to, via `getAutoAttachList().add()`.
    SetAutoAttach { process: String },
}

/// What the caller lends to a Lua call, and what comes back out.
pub struct LuaCtx<'a> {
    pub process: Option<&'a mut GameProcess>,
    pub symbols: &'a mut SymbolHandler,
    pub records: Vec<RecordView>,
    pub actions: Vec<LuaAction>,
}

/// Everything script execution needs besides the process itself.
///
/// Bundled into one struct so the Lua engine and the table snapshot can reach
/// the Auto Assembler without threading four extra parameters through every
/// activation call.
pub struct ExecEnv<'a> {
    pub symbols: &'a mut SymbolHandler,
    pub lua: &'a LuaEngine,
    /// The whole table as Lua sees it, snapshotted before execution.
    pub record_views: &'a [RecordView],
    /// Record changes Lua requested, applied by the caller afterwards.
    pub actions: Vec<LuaAction>,
    /// Children a `moActivateChildrenAsWell` parent wants brought up, handed
    /// back to the caller to schedule rather than activated inline.
    ///
    /// A subtree is not activated in one blocking sweep: an Auto Assembler
    /// record activates on a `TMemoryRecordProcessingThread`, `setActive`
    /// returns immediately, and the cascade happens later from `processingDone`
    /// once that script has finished. Nine scripts
    /// therefore go up spread over real time, not back to back on one thread.
    ///
    /// The queue is what keeps the AA engine from being re-entered mid-script,
    /// which is reason enough for it to exist. The per-tick *pacing* on top of
    /// it is a leftover: it was measured against a ptrace backend that stopped
    /// the game once per allocation, and nothing stops the game any more. See
    /// the note in `freeze_loop.rs` and CONTINUE.md.
    pub deferred_activations: Vec<(u32, bool)>,
}

impl<'a> ExecEnv<'a> {
    pub fn new(
        symbols: &'a mut SymbolHandler,
        lua: &'a LuaEngine,
        record_views: &'a [RecordView],
    ) -> Self {
        Self {
            symbols,
            lua,
            record_views,
            actions: Vec::new(),
            deferred_activations: Vec::new(),
        }
    }
}

/// Type-erased access to the caller's process and symbol table.
///
/// The API closures registered with Lua must be `'static`, so they cannot
/// borrow the caller's `&mut` state directly. They share this instead, and
/// [`Bridge::invalidate`] clears the pointers the moment the call finishes.
///
/// # Safety
///
/// The pointers are only dereferenced between `install` and the matching
/// `invalidate`, which bracket a single synchronous call on one thread. Any
/// function a script retains past that point sees nulls and errors cleanly.
struct Frame {
    process: *mut GameProcess,
    symbols: *mut SymbolHandler,
    /// The engine running this frame, so a nested `autoAssemble()` executes on
    /// the *same* Lua state instead of a fresh one. There is exactly one state
    /// (see the module docs); a nested script that got its own could not see
    /// the global `process`, nor any helper the table defined.
    lua: *const LuaEngine,
    records: Vec<RecordView>,
    actions: Vec<LuaAction>,
}

/// A stack of frames, one per in-flight call.
///
/// `autoAssemble()` runs a nested script from inside a Lua callback, so a
/// single frame would be torn down by the inner call while the outer one is
/// still running. Accessors always use the innermost frame.
#[derive(Default)]
struct Bridge {
    frames: Vec<Frame>,
}

impl Bridge {
    fn top(&self) -> mlua::Result<&Frame> {
        self.frames
            .last()
            .ok_or_else(|| mlua::Error::external("Lua API used outside of a script call"))
    }

    fn proc(&self) -> mlua::Result<&GameProcess> {
        let f = self.top()?;
        if f.process.is_null() {
            return Err(mlua::Error::external("not attached to a process"));
        }
        Ok(unsafe { &*f.process })
    }

    #[allow(clippy::mut_from_ref)]
    fn proc_mut(&self) -> mlua::Result<&mut GameProcess> {
        let f = self.top()?;
        if f.process.is_null() {
            return Err(mlua::Error::external("not attached to a process"));
        }
        Ok(unsafe { &mut *f.process })
    }

    fn proc_opt(&self) -> Option<&GameProcess> {
        let f = self.frames.last()?;
        if f.process.is_null() {
            None
        } else {
            Some(unsafe { &*f.process })
        }
    }

    fn syms(&self) -> mlua::Result<&SymbolHandler> {
        let f = self.top()?;
        if f.symbols.is_null() {
            return Err(mlua::Error::external("Lua API used outside of a script call"));
        }
        Ok(unsafe { &*f.symbols })
    }

    #[allow(clippy::mut_from_ref)]
    fn syms_mut(&self) -> mlua::Result<&mut SymbolHandler> {
        let f = self.top()?;
        if f.symbols.is_null() {
            return Err(mlua::Error::external("Lua API used outside of a script call"));
        }
        Ok(unsafe { &mut *f.symbols })
    }

    fn records(&self) -> &[RecordView] {
        self.frames.last().map(|f| f.records.as_slice()).unwrap_or(&[])
    }

    fn push_action(&mut self, a: LuaAction) {
        if let Some(f) = self.frames.last_mut() {
            f.actions.push(a);
        }
    }
}

/// # Safety
///
/// `Frame` holds raw pointers to the caller's stack. Every access is serialised
/// by the `Mutex` around the `Bridge` and by the runtime state lock above that,
/// and frames are popped before the call they belong to returns.
unsafe impl Send for Frame {}

type Shared = Arc<Mutex<Bridge>>;

pub struct LuaEngine {
    lua: Lua,
    shared: Shared,
    installed: std::cell::Cell<bool>,
}

// SAFETY: `Lua` is Send with mlua's `send` feature; `shared` is a Mutex whose
// contents are Send (see `unsafe impl Send for Frame`). The Cell is only
// touched while the runtime state lock is held.
unsafe impl Sync for LuaEngine {}

impl Default for LuaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaEngine {
    pub fn new() -> Self {
        Self {
            lua: Lua::new(),
            shared: Arc::new(Mutex::new(Bridge::default())),
            installed: std::cell::Cell::new(false),
        }
    }

    /// Drives Lua threads and timers created by `createThread`/`createTimer`.
    ///
    /// Called on the runtime's periodic tick. Each resume runs until the
    /// coroutine yields (which `sleep` does), so a thread body that loops
    /// forever does not block anything.
    pub fn tick(&self, ctx: &mut LuaCtx<'_>) -> mlua::Result<()> {
        // Skip the whole dance when nothing is scheduled.
        if !self.installed.get() {
            return Ok(());
        }
        let pending: bool = {
            let g = self.lua.globals();
            match g.get::<mlua::Function>("__htt_hasWork") {
                Ok(f) => f.call(()).unwrap_or(false),
                Err(_) => false,
            }
        };
        if !pending {
            return Ok(());
        }

        self.with_api(ctx, |lua| {
            let now = now_ms();
            let f: mlua::Function = lua.globals().get("__htt_tick")?;
            f.call::<()>(now)
        })
    }

    /// A callback whose first argument is the memory record it concerns.
    ///
    /// The `TMemoryRecord` itself is passed; the Lua-visible equivalent
    /// here is the same table
    /// `{$lua}` blocks receive as `memrec`.
    pub fn call_record_callback(
        &self,
        name: &str,
        rec: &RecordView,
        extra: Vec<Value>,
        ctx: &mut LuaCtx<'_>,
    ) -> bool {
        let name = name.to_string();
        let rec = rec.clone();
        let res = self.with_api(ctx, move |lua| {
            let f: Value = lua.globals().get(name.as_str())?;
            let Value::Function(f) = f else {
                return Ok(false);
            };
            let mut args = vec![build_record_table(lua, &rec)?];
            args.extend(extra);
            if let Err(e) = f.call::<Value>(MultiValue::from_vec(args)) {
                warn!("lua callback '{}' failed: {}", name, e);
            }
            Ok(true)
        });
        res.unwrap_or_else(|e| {
            warn!("could not invoke lua callback: {}", e);
            false
        })
    }

    /// A Lua global read as a string, for the `$name` notation.
    ///
    /// A token starting with `$` is evaluated as Lua -
    /// `lua_getglobal(name)`, falling back to `return <name>` as an expression -
    /// and a string result is then resolved as a symbol in its own right. The
    /// case that matters is `$process`, since `TProcessHandler.Open` publishes
    /// the target's module name as the global `process`, so `$process` means
    /// "the main module" without a table having to hardcode its name.
    ///
    /// Deliberately takes no `LuaCtx`: reading a global calls nothing, so it
    /// needs neither a process nor a symbol table, and demanding them would
    /// make this unusable from inside the Auto Assembler - which is exactly
    /// where tables use it.
    pub fn global_string(&self, name: &str) -> Option<String> {
        if let Ok(Some(s)) = self.lua.globals().get::<Option<String>>(name) {
            return Some(s);
        }
        // Not a plain global: it is then tried as an expression.
        self.lua
            .load(format!("return {}", name))
            .eval::<Option<String>>()
            .ok()
            .flatten()
    }

    /// Runs a `{$lua}` block. Returns the string it produced, if any, which the
    /// caller splices back into the script as assembler source.
    pub fn run_block(
        &self,
        code: &str,
        memrec: Option<&RecordView>,
        ctx: &mut LuaCtx<'_>,
    ) -> mlua::Result<Option<String>> {
        let memrec = memrec.cloned();
        self.with_api(ctx, move |lua| {
            let chunk = format!("local syntaxcheck,memrec=...\n{}", code);
            let f = lua.load(&chunk).into_function()?;

            let memrec_val = match memrec {
                Some(ref r) => build_record_table(lua, r)?,
                None => Value::Nil,
            };

            let args = MultiValue::from_vec(vec![Value::Boolean(false), memrec_val]);
            let ret: MultiValue = f.call(args)?;
            match ret.into_iter().next() {
                Some(Value::String(s)) => Ok(Some(s.to_str()?.to_string())),
                _ => Ok(None),
            }
        })
    }

    /// Sets a global string, for values injected before a table script runs.
    pub fn set_global_string(&self, name: &str, value: &str) -> mlua::Result<()> {
        self.lua.globals().set(name, value)
    }

    /// As above, for the boolean flags set with `LUA_DoScript('x=true')`.
    pub fn set_global_bool(&self, name: &str, value: bool) -> mlua::Result<()> {
        self.lua.globals().set(name, value)
    }

    /// Invokes a table-defined callback, the way the `LUA_functioncall` does.
    ///
    /// Two properties of that function are the contract here, both
    /// load-bearing: it guards with `lua_isfunction`, so a table that
    /// never defines the callback is not an error; and it calls through
    /// `lua_pcall`, so an error *inside* the callback cannot escape into the
    /// engine. Callbacks are the table's code running on our thread - a broken
    /// one must not take the trainer down with it.
    ///
    /// Returns whether the callback existed and ran to completion.
    pub fn call_callback(
        &self,
        name: &str,
        args: Vec<Value>,
        ctx: &mut LuaCtx<'_>,
    ) -> bool {
        let name = name.to_string();
        let res = self.with_api(ctx, move |lua| {
            let f: Value = lua.globals().get(name.as_str())?;
            let Value::Function(f) = f else {
                return Ok(false);
            };
            match f.call::<Value>(MultiValue::from_vec(args)) {
                Ok(_) => Ok(true),
                Err(e) => {
                    // the lua_pcall swallows this too; log rather than raise.
                    warn!("lua callback '{}' failed: {}", name, e);
                    Ok(true)
                }
            }
        });
        match res {
            Ok(ran) => ran,
            Err(e) => {
                warn!("could not invoke lua callback: {}", e);
                false
            }
        }
    }

    /// Compiles a block without running it, to check syntax only.
    pub fn syntax_check(&self, code: &str) -> mlua::Result<()> {
        let chunk = format!("local syntaxcheck,memrec=...\n{}", code);
        self.lua.load(&chunk).into_function().map(|_| ())
    }

    /// Aborts any Lua that runs longer than `limit` VM instructions.
    ///
    /// Only for offline analysis, where a table with a top-level `while true`
    /// would otherwise hang the whole sweep.
    pub fn set_instruction_limit(&self, limit: u32) {
        use std::sync::atomic::{AtomicU32, Ordering};
        let budget = Arc::new(AtomicU32::new(limit));
        let reset = limit;
        self.lua.set_hook(
            mlua::HookTriggers::new().every_nth_instruction(10_000),
            move |_, _| {
                let left = budget.fetch_sub(1, Ordering::Relaxed);
                if left == 0 {
                    budget.store(reset, Ordering::Relaxed);
                    return Err(mlua::Error::external("script exceeded the instruction budget"));
                }
                Ok(mlua::VmState::Continue)
            },
        );
    }

    /// Runs a `LuaCall(...)` expression for its side effects.
    pub fn run_call(&self, expr: &str, ctx: &mut LuaCtx<'_>) -> mlua::Result<()> {
        let expr = expr.to_string();
        self.with_api(ctx, move |lua| {
            lua.load(&expr).exec()?;
            Ok(())
        })
    }

    /// Installs the table API over `ctx`, runs `body`, then tears the bridge down
    /// and copies results back into `ctx`.
    fn with_api<R>(
        &self,
        ctx: &mut LuaCtx<'_>,
        body: impl FnOnce(&Lua) -> mlua::Result<R>,
    ) -> mlua::Result<R> {
        // Globals are installed once and bound to the frame stack, so nested
        // calls reuse them rather than rebinding to a dead frame.
        if !self.installed.get() {
            let shared = Arc::clone(&self.shared);
            self.install(&shared)?;
            self.lua.load(SCHEDULER_PRELUDE).exec()?;
            self.lua.load(SCANNER_PRELUDE).exec()?;
            self.installed.set(true);
        }

        self.shared.lock().unwrap().frames.push(Frame {
            process: match ctx.process.as_deref_mut() {
                Some(p) => p as *mut GameProcess,
                None => std::ptr::null_mut(),
            },
            symbols: ctx.symbols as *mut SymbolHandler,
            lua: self as *const LuaEngine,
            records: std::mem::take(&mut ctx.records),
            actions: Vec::new(),
        });

        let result = body(&self.lua);

        // Always pop, even on error, so retained functions cannot reach the
        // caller's state after this returns.
        if let Some(mut frame) = self.shared.lock().unwrap().frames.pop() {
            ctx.records = std::mem::take(&mut frame.records);
            ctx.actions.append(&mut frame.actions);
        }

        result
    }

    fn install(&self, shared: &Shared) -> mlua::Result<()> {
        let lua = &self.lua;
        let g = lua.globals();

        // ---- memory reads ---------------------------------------------
        macro_rules! reader {
            ($name:literal, $len:expr, $conv:expr) => {{
                let s = Arc::clone(shared);
                g.set(
                    $name,
                    lua.create_function(move |_, addr: Value| {
                        let b = s.lock().unwrap();
                        let p = b.proc()?;
                        let a = resolve_addr(&addr, Some(p), b.syms()?)?;
                        let mut buf = [0u8; $len];
                        p.read_memory(a, &mut buf).map_err(mlua::Error::external)?;
                        #[allow(clippy::redundant_closure_call)]
                        Ok($conv(buf))
                    })?,
                )?;
            }};
        }

        reader!("readInteger", 4, |b: [u8; 4]| i32::from_le_bytes(b) as i64);
        reader!("readDword", 4, |b: [u8; 4]| i32::from_le_bytes(b) as i64);
        reader!("readSmallInteger", 2, |b: [u8; 2]| i16::from_le_bytes(b) as i64);
        reader!("readWord", 2, |b: [u8; 2]| i16::from_le_bytes(b) as i64);
        reader!("readByte", 1, |b: [u8; 1]| b[0] as i64);
        reader!("readQword", 8, |b: [u8; 8]| i64::from_le_bytes(b));
        reader!("readFloat", 4, |b: [u8; 4]| f32::from_le_bytes(b) as f64);
        reader!("readDouble", 8, |b: [u8; 8]| f64::from_le_bytes(b));

        // ---- memory writes --------------------------------------------
        macro_rules! writer {
            ($name:literal, $ty:ty, $to:expr) => {{
                let s = Arc::clone(shared);
                g.set(
                    $name,
                    lua.create_function(move |_, (addr, v): (Value, f64)| {
                        let b = s.lock().unwrap();
                        let p = b.proc()?;
                        let a = resolve_addr(&addr, Some(p), b.syms()?)?;
                        #[allow(clippy::redundant_closure_call)]
                        let bytes = $to(v as $ty);
                        p.write_memory(a, &bytes).map_err(mlua::Error::external)?;
                        Ok(true)
                    })?,
                )?;
            }};
        }

        writer!("writeInteger", i32, |v: i32| v.to_le_bytes().to_vec());
        writer!("writeDword", i32, |v: i32| v.to_le_bytes().to_vec());
        writer!("writeSmallInteger", i16, |v: i16| v.to_le_bytes().to_vec());
        writer!("writeWord", i16, |v: i16| v.to_le_bytes().to_vec());
        writer!("writeByte", u8, |v: u8| vec![v]);
        writer!("writeQword", i64, |v: i64| v.to_le_bytes().to_vec());

        // ---- pointer-width access -------------------------------------
        // A pointer is 8 bytes in a 64-bit target and 4 in a WoW64 one;
        // reading 8 from a 32-bit game pulls in whatever follows the pointer.
        let s = Arc::clone(shared);
        g.set(
            "readPointer",
            lua.create_function(move |_, addr: Value| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                let a = resolve_addr(&addr, Some(p), b.syms()?)?;
                if p.is_64bit {
                    let mut buf = [0u8; 8];
                    p.read_memory(a, &mut buf).map_err(mlua::Error::external)?;
                    Ok(i64::from_le_bytes(buf))
                } else {
                    let mut buf = [0u8; 4];
                    p.read_memory(a, &mut buf).map_err(mlua::Error::external)?;
                    Ok(u32::from_le_bytes(buf) as i64)
                }
            })?,
        )?;
        let s = Arc::clone(shared);
        g.set(
            "writePointer",
            lua.create_function(move |_, (addr, v): (Value, f64)| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                let a = resolve_addr(&addr, Some(p), b.syms()?)?;
                let bytes = if p.is_64bit {
                    (v as i64).to_le_bytes().to_vec()
                } else {
                    (v as i64 as u32).to_le_bytes().to_vec()
                };
                p.write_memory(a, &bytes).map_err(mlua::Error::external)?;
                Ok(true)
            })?,
        )?;
        // targetIs64Bit(): tables branch on this to pick a pointer width or
        // a script variant.
        let s = Arc::clone(shared);
        g.set(
            "targetIs64Bit",
            lua.create_function(move |_, ()| {
                let b = s.lock().unwrap();
                Ok(b.proc()?.is_64bit)
            })?,
        )?;
        writer!("writeFloat", f32, |v: f32| v.to_le_bytes().to_vec());
        writer!("writeDouble", f64, |v: f64| v.to_le_bytes().to_vec());

        // readBytes(address, count [, returnAsTable])
        let s = Arc::clone(shared);
        g.set(
            "readBytes",
            lua.create_function(move |lua, args: Variadic<Value>| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                let addr = args
                    .first()
                    .ok_or_else(|| mlua::Error::external("readBytes: missing address"))?;
                let a = resolve_addr(addr, Some(p), b.syms()?)?;
                let n = match args.get(1) {
                    Some(Value::Integer(i)) => *i as usize,
                    Some(Value::Number(f)) => *f as usize,
                    _ => 1,
                };
                let as_table = matches!(args.get(2), Some(Value::Boolean(true)));
                let mut buf = vec![0u8; n.max(1)];
                p.read_memory(a, &mut buf).map_err(mlua::Error::external)?;

                if as_table || n > 1 {
                    let t = lua.create_table()?;
                    for (i, byte) in buf.iter().enumerate() {
                        t.set(i + 1, *byte as i64)?;
                    }
                    Ok(Value::Table(t))
                } else {
                    Ok(Value::Integer(buf[0] as i64))
                }
            })?,
        )?;

        // writeBytes(address, b1, b2, ...) or writeBytes(address, table)
        let s = Arc::clone(shared);
        g.set(
            "writeBytes",
            lua.create_function(move |_, args: Variadic<Value>| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                let addr = args
                    .first()
                    .ok_or_else(|| mlua::Error::external("writeBytes: missing address"))?;
                let a = resolve_addr(addr, Some(p), b.syms()?)?;

                let mut bytes: Vec<u8> = Vec::new();
                for v in args.iter().skip(1) {
                    match v {
                        Value::Integer(i) => bytes.push(*i as u8),
                        Value::Number(f) => bytes.push(*f as u8),
                        Value::Table(t) => {
                            for item in t.clone().sequence_values::<i64>() {
                                bytes.push(item? as u8);
                            }
                        }
                        _ => {}
                    }
                }
                if bytes.is_empty() {
                    return Ok(false);
                }
                p.write_memory(a, &bytes).map_err(mlua::Error::external)?;
                Ok(true)
            })?,
        )?;

        let s = Arc::clone(shared);
        g.set(
            "readString",
            lua.create_function(move |_, (addr, n): (Value, Option<usize>)| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                let a = resolve_addr(&addr, Some(p), b.syms()?)?;
                let mut buf = vec![0u8; n.unwrap_or(64)];
                p.read_memory(a, &mut buf).map_err(mlua::Error::external)?;
                let end = buf.iter().position(|x| *x == 0).unwrap_or(buf.len());
                Ok(String::from_utf8_lossy(&buf[..end]).to_string())
            })?,
        )?;

        let s = Arc::clone(shared);
        g.set(
            "writeString",
            lua.create_function(move |_, (addr, text): (Value, String)| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                let a = resolve_addr(&addr, Some(p), b.syms()?)?;
                let mut bytes = text.into_bytes();
                bytes.push(0);
                p.write_memory(a, &bytes).map_err(mlua::Error::external)?;
                Ok(true)
            })?,
        )?;

        // ---- addresses and symbols ------------------------------------
        let s = Arc::clone(shared);
        g.set(
            "getAddress",
            lua.create_function(move |_, name: Value| {
                let b = s.lock().unwrap();
                let a = resolve_addr(&name, b.proc_opt(), b.syms()?)?;
                Ok(a as i64)
            })?,
        )?;

        // Size of a loaded module - the companion to getAddress. Tables pair
        // the two to bound a scan to the main image:
        //   getAddress(process) .. getAddress(process) + getModuleSize(process)
        let s = Arc::clone(shared);
        g.set(
            "getModuleSize",
            lua.create_function(move |_, name: String| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                let key = name.trim().trim_matches('"').to_lowercase();
                match p.modules.get(&key) {
                    Some(m) => Ok(Value::Integer(m.size as i64)),
                    None => Ok(Value::Nil),
                }
            })?,
        )?;

        // The one primitive behind the scan classes. `createMemScan` and
        // `createFoundList` are built on top of this in Lua (SCANNER_PRELUDE)
        // because they are mostly bookkeeping; this is the part that has to
        // reach memory. Returns raw addresses - the Lua side formats them the
        // way the foundlist does.
        let s = Arc::clone(shared);
        g.set(
            "__htt_aobScanRange",
            lua.create_function(move |lua, (pattern, start, stop): (String, i64, i64)| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                let pat = crate::aob_scanner::parse_aob_pattern(&pattern)
                    .map_err(mlua::Error::external)?;
                let hits = crate::aob_scanner::aob_scan_range(
                    p,
                    &pat,
                    start.max(0) as usize,
                    stop.max(0) as usize,
                    1024,
                )
                .map_err(mlua::Error::external)?;

                let t = lua.create_table()?;
                for (i, a) in hits.iter().enumerate() {
                    t.set(i + 1, *a as i64)?;
                }
                Ok(t)
            })?,
        )?;

        // the readBytes(..., true) hands back a byte *table*; tables that scan
        // by hand turn it into a string to search with string.find. Latin-1, so
        // every byte maps to one char and survives the round trip.
        g.set(
            "byteTableToString",
            lua.create_function(|_, t: mlua::Table| {
                let mut out = String::with_capacity(t.raw_len());
                for b in t.sequence_values::<i64>() {
                    out.push(((b? & 0xFF) as u8) as char);
                }
                Ok(out)
            })?,
        )?;

        let s = Arc::clone(shared);
        g.set(
            "getAddressSafe",
            lua.create_function(move |_, name: Value| {
                let b = s.lock().unwrap();
                match resolve_addr(&name, b.proc_opt(), b.syms()?) {
                    Ok(a) => Ok(Value::Integer(a as i64)),
                    Err(_) => Ok(Value::Nil),
                }
            })?,
        )?;

        let s = Arc::clone(shared);
        g.set(
            "registerSymbol",
            lua.create_function(move |_, (name, addr): (String, Value)| {
                let b = s.lock().unwrap();
                let a = resolve_addr(&addr, b.proc_opt(), b.syms()?)?;
                b.syms_mut()?.register_symbol(&name, a);
                Ok(())
            })?,
        )?;

        let s = Arc::clone(shared);
        g.set(
            "unregisterSymbol",
            lua.create_function(move |_, name: String| {
                s.lock().unwrap().syms_mut()?.unregister_symbol(&name);
                Ok(())
            })?,
        )?;

        // ---- AOB scanning ---------------------------------------------
        let s = Arc::clone(shared);
        g.set(
            "AOBScan",
            lua.create_function(move |lua, args: Variadic<Value>| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                // Accepts one pattern string or byte-per-argument.
                let pat = args
                    .iter()
                    .filter_map(|v| match v {
                        Value::String(x) => x.to_str().ok().map(|y| y.to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let pattern = parse_aob_pattern(&pat).map_err(mlua::Error::external)?;
                scan_result(lua, aob_scan(p, &pattern, None).ok())
            })?,
        )?;

        let s = Arc::clone(shared);
        g.set(
            "AOBScanModule",
            lua.create_function(move |lua, (module, pat): (String, String)| {
                let b = s.lock().unwrap();
                let p = b.proc()?;
                let pattern = parse_aob_pattern(&pat).map_err(mlua::Error::external)?;
                scan_result(lua, aob_scan(p, &pattern, Some(&module)).ok())
            })?,
        )?;

        // ---- memory records -------------------------------------------
        // the luaclass_addClassFunctionToTable registers every method twice:
        // as written and with the first letter uppercased. Tables rely on both
        // spellings (`getMemoryRecordByID` and `GetMemoryRecordByID`).
        fn set_both(t: &Table, name: &str, f: mlua::Function) -> mlua::Result<()> {
            t.set(name, f.clone())?;
            let mut c = name.chars();
            if let Some(first) = c.next() {
                let cap: String = first.to_uppercase().collect::<String>() + c.as_str();
                if cap != name {
                    t.set(cap, f)?;
                }
            }
            Ok(())
        }

        let addresslist = lua.create_table()?;

        let s = Arc::clone(shared);
        set_both(
            &addresslist,
            "getMemoryRecordByID",
            lua.create_function(move |lua, args: Variadic<Value>| {
                // Works for both `al.getMemoryRecordByID(id)` and `al:...(id)`.
                let id = last_integer(&args);
                let b = s.lock().unwrap();
                match b.records().iter().find(|r| Some(r.id as i64) == id) {
                    Some(r) => build_record_table(lua, r),
                    None => Ok(Value::Nil),
                }
            })?,
        )?;

        let s = Arc::clone(shared);
        set_both(
            &addresslist,
            "getMemoryRecordByDescription",
            lua.create_function(move |lua, args: Variadic<Value>| {
                let want = args.iter().rev().find_map(|v| match v {
                    Value::String(x) => x.to_str().ok().map(|y| y.to_string()),
                    _ => None,
                });
                let b = s.lock().unwrap();
                match want
                    .and_then(|w| b.records().iter().find(|r| r.description.eq_ignore_ascii_case(&w)))
                {
                    Some(r) => build_record_table(lua, r),
                    None => Ok(Value::Nil),
                }
            })?,
        )?;

        let s = Arc::clone(shared);
        set_both(
            &addresslist,
            "getCount",
            lua.create_function(move |_, _: Variadic<Value>| Ok(s.lock().unwrap().records().len() as i64))?,
        )?;
        addresslist.set("Count", 0i64)?;

        // Unknown fields on the address list are GUI properties (`.Control`,
        // `.Header`, ...). Yield a permissive dummy so headless table scripts
        // that poke at the UI do not abort, matching getMainForm().
        let al_meta = lua.create_table()?;
        al_meta.set(
            "__index",
            lua.create_function(|lua, _: (Table, Value)| dummy_object(lua))?,
        )?;
        addresslist.set_metatable(Some(al_meta));

        g.set(
            "getAddressList",
            lua.create_function(move |_, _: Variadic<Value>| Ok(addresslist.clone()))?,
        )?;

        // Activation is queued; see the module docs.
        let s = Arc::clone(shared);
        g.set(
            "__htt_setRecordActive",
            lua.create_function(move |_, (id, active): (i64, bool)| {
                s.lock().unwrap().push_action(LuaAction::SetActive { id: id as u32, active });
                Ok(())
            })?,
        )?;

        let s = Arc::clone(shared);
        g.set(
            "__htt_setRecordValue",
            lua.create_function(move |_, (id, value): (i64, String)| {
                s.lock().unwrap().push_action(LuaAction::SetValue { id: id as u32, value });
                Ok(())
            })?,
        )?;

        let s = Arc::clone(shared);
        g.set(
            "__htt_recordById",
            lua.create_function(move |lua, id: i64| {
                let b = s.lock().unwrap();
                match b.records().iter().find(|r| r.id as i64 == id) {
                    Some(r) => build_record_table(lua, r),
                    None => Ok(Value::Nil),
                }
            })?,
        )?;

        let s = Arc::clone(shared);
        g.set(
            "__htt_getRecordAddress",
            lua.create_function(move |_, id: i64| {
                let b = s.lock().unwrap();
                let Some(rec) = b.records().iter().find(|r| r.id as i64 == id) else {
                    return Ok(Value::Nil);
                };
                if rec.address_expr.is_empty() {
                    return Ok(Value::Nil);
                }
                let Some(p) = b.proc_opt() else { return Ok(Value::Nil) };
                match evaluate_address(&rec.address_expr, Some(p), b.syms()?) {
                    Ok(a) => Ok(Value::Integer(a as i64)),
                    Err(_) => Ok(Value::Nil),
                }
            })?,
        )?;

        // ---- assorted ---------------------------------------------------
        // autoAssemble(script) runs a nested AA script. The frame stack makes
        // this safe: the inner call pushes its own frame and pops it again.
        let s = Arc::clone(shared);
        g.set(
            "autoAssemble",
            lua.create_function(move |_, args: Variadic<Value>| {
                let Some(Value::String(src)) = args.first() else {
                    return Ok(false);
                };
                let script_text = src.to_str()?.to_string();

                let (proc_ptr, sym_ptr, lua_ptr, records) = {
                    let b = s.lock().unwrap();
                    let f = b.top()?;
                    (f.process, f.symbols, f.lua, f.records.clone())
                };
                if proc_ptr.is_null() {
                    return Err(mlua::Error::external("autoAssemble: not attached"));
                }

                // SAFETY: the pointers come from the live frame, which outlives
                // this call.
                let process = unsafe { &mut *proc_ptr };
                let symbols = unsafe { &mut *sym_ptr };

                let mut script = crate::auto_assembler::AutoAssemblerScript::parse(&script_text);

                // Run on the caller's engine, never a new one. There is a
                // single Lua state, and a nested script depends on it: RDR2's
                // BlipPTR does `aobscanmodule(BlipPTR,$process,...)` from
                // inside `autoAssemble([[...]])`, and `$process` reads the
                // global that `TProcessHandler.Open` publishes. With a fresh
                // state that
                // global does not exist, the module argument cannot resolve,
                // and the scan fails - which is what stopped BlipPTR enabling.
                // Any helper the table defined would be missing for the same
                // reason.
                //
                // SAFETY: same argument as `process`/`symbols` above - the
                // pointer comes from the live frame, which is the engine
                // currently executing this callback.
                let engine = unsafe { &*lua_ptr };
                let mut env = ExecEnv::new(symbols, engine, &records);
                match script.execute_enable(process, &mut env, None) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        warn!("autoAssemble() failed: {}", e);
                        Ok(false)
                    }
                }
            })?,
        )?;

        // Blocking sleep, used by the Lua-side `sleep` when not in a coroutine.
        g.set(
            "__htt_realsleep",
            lua.create_function(|_, ms: u64| {
                std::thread::sleep(std::time::Duration::from_millis(ms.min(2_000)));
                Ok(())
            })?,
        )?;

        g.set(
            "__htt_log",
            lua.create_function(|_, msg: String| {
                warn!("[lua] {}", msg);
                Ok(())
            })?,
        )?;

        // ---- window / input helpers ------------------------------------
        // RDR2's teleport scripts drive the game window directly.
        g.set(
            "findWindow",
            lua.create_function(|_, args: Variadic<Value>| {
                let title = args.iter().rev().find_map(|v| match v {
                    Value::String(s) => s.to_str().ok().map(|x| x.to_string()),
                    _ => None,
                });
                Ok(find_window(title.as_deref()) as i64)
            })?,
        )?;
        g.set(
            "getForegroundWindow",
            lua.create_function(|_, ()| Ok(get_foreground_window() as i64))?,
        )?;
        g.set(
            "setForegroundWindow",
            lua.create_function(|_, h: i64| {
                set_foreground_window(h as isize);
                Ok(true)
            })?,
        )?;
        g.set(
            "mouse_event",
            lua.create_function(|_, args: Variadic<Value>| {
                let flags = match args.first() {
                    Some(Value::Integer(i)) => *i as u32,
                    Some(Value::Number(f)) => *f as u32,
                    _ => 0,
                };
                send_mouse_event(flags);
                Ok(())
            })?,
        )?;
        for (name, val) in [
            ("MOUSEEVENTF_LEFTDOWN", 0x0002i64),
            ("MOUSEEVENTF_LEFTUP", 0x0004),
            ("MOUSEEVENTF_RIGHTDOWN", 0x0008),
            ("MOUSEEVENTF_RIGHTUP", 0x0010),
            ("MOUSEEVENTF_MIDDLEDOWN", 0x0020),
            ("MOUSEEVENTF_MIDDLEUP", 0x0040),
            ("MOUSEEVENTF_MOVE", 0x0001),
            ("MOUSEEVENTF_ABSOLUTE", 0x8000),
        ] {
            g.set(name, val)?;
        }

        // This exposes a call to an exported function in the target. There is
        // no code-injection thread here, so treat the common
        // `executeCodeLocal("SetForegroundWindow", hwnd)` shape specially and
        // report anything else rather than pretending it ran.
        g.set(
            "executeCodeLocal",
            lua.create_function(|_, args: Variadic<Value>| {
                let name = match args.first() {
                    Some(Value::String(s)) => s.to_str().map(|x| x.to_string()).unwrap_or_default(),
                    _ => String::new(),
                };
                if name.eq_ignore_ascii_case("SetForegroundWindow") {
                    if let Some(Value::Integer(h)) = args.get(1) {
                        set_foreground_window(*h as isize);
                        return Ok(1i64);
                    }
                }
                warn!("executeCodeLocal('{}') is not supported in this runtime", name);
                Ok(0i64)
            })?,
        )?;

        g.set(
            "shellExecute",
            lua.create_function(|_, args: Variadic<Value>| {
                let cmd = match args.first() {
                    Some(Value::String(s)) => s.to_str().map(|x| x.to_string()).unwrap_or_default(),
                    _ => String::new(),
                };
                // Tables use this to open URLs and readme files. Log it, since
                // it launches a program on the user's machine.
                info!("shellExecute: {}", cmd);
                shell_execute(&cmd);
                Ok(true)
            })?,
        )?;

        // the speedhack rewrites the target's timing functions. That is a
        // separate feature from anything here, so say so rather than silently
        // doing nothing.
        for name in ["speedhack_setSpeed", "speedhack_getSpeed"] {
            g.set(
                name,
                lua.create_function(move |_, _: Variadic<Value>| {
                    warn!("speedhack is not implemented in this runtime");
                    Ok(1.0f64)
                })?,
            )?;
        }

        for name in [
            "createMenuItem",
            "getMainForm",
            "getLuaEngine",
            "createForm",
            "createPanel",
            "createLabel",
            "createButton",
            "createHotkey",
            "getSettings",
            "createMemo",
            "createEdit",
            "createCheckBox",
            "createGroupBox",
            "createImage",
            "createTimerObject",
            "createStringlist",
            "createTrackBar",
            "createComboBox",
            "createListBox",
            "createTreeView",
            "createBitmap",
            "createPicture",
            "createFont",
            "createHotkeyObject",
            "changeHotkeyKeysForm",
            "getHotkeyByID",
            "convertKeyComboToString",
            "convertKeyComboToKeys",
            "messageDialog",
            "inputQuery",
            "playSound",
            "findTableFile",
            "fixDPI",
            "getScreenDPI",
            "getFormsList",
            "getTrainerForm",
            "getFoundList",
            "getMemoryViewForm",
            "getSettingsForm",
        ] {
            g.set(
                name,
                lua.create_function(|lua, _: Variadic<Value>| dummy_object(lua))?,
            )?;
        }

        g.set("getTickCount", lua.create_function(|_, ()| Ok(now_ms()))?)?;

        // The scheduler arms a timer's first deadline the moment it is enabled,
        // which means Lua needs the *same* clock `tick` passes in - a different
        // epoch would make every deadline either instantly due or unreachable.
        g.set("__htt_now", lua.create_function(|_, ()| Ok(now_ms()))?)?;

        g.set("getCEVersion", lua.create_function(|_, ()| Ok(7.5f64))?)?;

        // A table calling closeCE() means "close the trainer UI". There is no
        // UI here, and exiting would kill the REST server, so just note it.
        g.set(
            "closeCE",
            lua.create_function(|_, _: Variadic<Value>| {
                info!("table called closeCE(); ignoring in headless mode");
                Ok(())
            })?,
        )?;

        // Each API name is registered in more than one casing, and tables rely
        // on it (`GetAddressList` alongside `getAddressList`). Alias every
        // engine global to its first-letter-uppercase form, without touching
        // anything already defined.
        let api_names: Vec<String> = g
            .clone()
            .pairs::<String, Value>()
            .filter_map(|p| p.ok())
            .filter(|(k, v)| {
                matches!(v, Value::Function(_)) && k.starts_with(|c: char| c.is_lowercase())
            })
            .map(|(k, _)| k)
            .collect();
        for name in api_names {
            let mut chars = name.chars();
            let Some(first) = chars.next() else { continue };
            let cap: String = first.to_uppercase().collect::<String>() + chars.as_str();
            if g.get::<Value>(cap.as_str()).map(|v| v.is_nil()).unwrap_or(true) {
                let f: Value = g.get(name.as_str())?;
                g.set(cap, f)?;
            }
        }

        // getAutoAttachList(): the table declares which process the trainer
        // should latch onto. Wire it to the runtime's auto-attach target so a
        // generated trainer connects on its own, as the does.
        let s = Arc::clone(shared);
        let auto_list = lua.create_table()?;
        let adder = lua.create_function(move |_, args: Variadic<Value>| {
            if let Some(name) = args.iter().rev().find_map(|v| match v {
                Value::String(x) => x.to_str().ok().map(|y| y.to_string()),
                _ => None,
            }) {
                info!("table requested auto-attach to '{}'", name);
                s.lock()
                    .unwrap()
                    .push_action(LuaAction::SetAutoAttach { process: name });
            }
            Ok(())
        })?;
        auto_list.set("add", adder.clone())?;
        auto_list.set("Add", adder)?;
        let aal_meta = lua.create_table()?;
        aal_meta.set(
            "__index",
            lua.create_function(|lua, _: (Table, Value)| dummy_object(lua))?,
        )?;
        auto_list.set_metatable(Some(aal_meta));
        g.set(
            "getAutoAttachList",
            lua.create_function(move |_, _: Variadic<Value>| Ok(auto_list.clone()))?,
        )?;

        // GUI singletons are also exposed as global *objects*, not functions.
        // Table scripts style the UI through these before doing anything else.
        for name in [
            "MainForm",
            "AddressList",
            "MemoryBrowser",
            "MemoryViewForm",
            "SettingsForm",
            "TrainerForm",
            "CETrainer",
            "Screen",
            "Application",
            "Clipboard",
        ] {
            if g.get::<Value>(name).map(|v| v.is_nil()).unwrap_or(true) {
                g.set(name, dummy_object(lua)?)?;
            }
        }

        for name in ["print", "showMessage", "ShowMessage", "outputDebugString"] {
            g.set(
                name,
                lua.create_function(|_, args: Variadic<Value>| {
                    let msg = args.iter().map(fmt_value).collect::<Vec<_>>().join("\t");
                    info!("[lua] {}", msg);
                    Ok(())
                })?,
            )?;
        }

        Ok(())
    }
}

/// Lua-side scheduler for `createThread` / `createTimer`.
///
/// Thread bodies conventionally run on real OS threads. Here one is a coroutine
/// and `sleep` yields, so the runtime can resume it on the next tick. That
/// gives the same observable behaviour for the standard
/// `while not t.Terminated do ... sleep(n) end` shape, without a thread holding
/// the Lua state for its whole life.
/// The memory-scan classes, assembled on top of `__htt_aobScanRange`.
///
/// These are mostly bookkeeping - a scan holds a range and a result set, a
/// found list reads it back - so they live in Lua rather than as more mlua
/// userdata. Only the scan itself needs Rust.
///
/// The shapes are taken from the reference behaviour, not guessed:
///
/// * `TFoundList.getAddress` returns a **hex string**, not a number
///   (via `inttohex(...,8)`) - which is why tables written
///   against that convention do `tonumber(list[0], 16)`.
/// * Found lists and string lists index from **0** and expose `Count`.
/// * `firstScan(scanOption, varType, roundingType, input1, input2,
///   startAddress, stopAddress, ...)`, so the pattern is argument 4 and the
///   range is arguments 6 and 7.
/// * The constants are the canonical values: `soExactValue=1`,
///   `vtByteArray=8`, `fsmNotAligned=0`.
///
/// Only `soExactValue` + `vtByteArray` - an AOB scan - is implemented. That is
/// what the `aobScanEx` helper in Otis_Inf / SunBeam tables uses, and it is the
/// only scan a headless trainer needs. Anything else **raises**, rather than
/// quietly returning no matches, which would read as "the signature is gone".
const SCANNER_PRELUDE: &str = r#"
soUnknownValue, soExactValue, soValueBetween, soBiggerThan, soSmallerThan = 0, 1, 2, 3, 4
vtByte, vtWord, vtDword, vtQword, vtSingle, vtDouble = 0, 1, 2, 3, 4, 5
vtString, vtUnicodeString, vtByteArray, vtBinary, vtAll = 6, 7, 8, 9, 10
fsmNotAligned, fsmAligned, fsmLastDigits = 0, 1, 2

function createStringlist()
  local items = {}
  local obj = {}
  obj.add = function(v) items[#items + 1] = v; return #items - 1 end
  obj.Add = obj.add
  obj.clear = function() items = {} end
  obj.Clear = obj.clear
  obj.getString = function(i) return items[i + 1] end
  obj.GetString = obj.getString
  obj.remove = function(i) table.remove(items, i + 1) end
  obj.destroy = function() items = {} end
  obj.Destroy = obj.destroy
  return setmetatable(obj, {
    __index = function(_, k)
      if type(k) == 'number' then return items[k + 1] end
      if k == 'Count' or k == 'count' then return #items end
      return nil
    end,
    __len = function() return #items end,
  })
end

function createMemScan(...)
  local scan = { __results = {} }
  scan.firstScan = function(scanOption, varType, roundingType, input1, input2,
                            startAddress, stopAddress, ...)
    if scanOption ~= soExactValue or varType ~= vtByteArray then
      error('createMemScan: only soExactValue + vtByteArray (an AOB scan) is implemented'
            .. ' in this engine; got scanOption=' .. tostring(scanOption)
            .. ' varType=' .. tostring(varType))
    end
    scan.__results = __htt_aobScanRange(input1, startAddress or 0, stopAddress or 0)
  end
  scan.FirstScan = scan.firstScan
  scan.waitTillDone = function() end
  scan.WaitTillDone = scan.waitTillDone
  scan.destroy = function() scan.__results = {} end
  scan.Destroy = scan.destroy
  return scan
end

function createFoundList(memscan)
  local list = { __scan = memscan }
  list.initialize = function() end
  list.Initialize = list.initialize
  list.getCount = function() return #(list.__scan.__results or {}) end
  list.GetCount = list.getCount
  list.getAddress = function(i)
    local a = (list.__scan.__results or {})[i + 1]
    if a == nil then return nil end
    return string.format('%.8X', a)
  end
  list.GetAddress = list.getAddress
  list.destroy = function() end
  list.Destroy = list.destroy
  return setmetatable(list, {
    __index = function(_, k)
      if k == 'Count' or k == 'count' then return list.getCount() end
      return nil
    end,
  })
end
"#;

const SCHEDULER_PRELUDE: &str = r#"
__htt_threads = __htt_threads or {}
__htt_timers  = __htt_timers  or {}

function createThread(f, ...)
  local obj = { Terminated = false, Name = '' }
  obj.terminate = function() obj.Terminated = true end
  obj.Terminate = obj.terminate
  obj.destroy   = obj.terminate
  obj.Destroy   = obj.terminate
  obj.freeOnTerminate = function() end
  obj.waitfor   = function() end

  if type(f) ~= 'function' then return obj end
  local co = coroutine.create(function() return f(obj) end)
  __htt_threads[#__htt_threads + 1] = { co = co, wake = 0, obj = obj }
  return obj
end
createThreadSuspended = createThread

-- the createTimer builds an LCL TTimer, so TCustomTimer is
-- what defines the semantics here, not intuition. Its UpdateTimer reads:
--
--   KillTimer;
--   if (FEnabled) and (FInterval > 0) and (...) and Assigned(FOnTimer) then
--     FTimerHandle := WidgetSet.CreateTimer(FInterval, @Timer);
--
-- Three consequences, all of which this mirrors:
--   * the handler is never called on enable - CreateTimer schedules the *first*
--     callback Interval ms out. A table that arms a 1000 ms timer expects a
--     full second of quiet, and RDR2's onOpenProcess uses that second to let
--     the process settle before nine AOB-scanning scripts run.
--   * Interval = 0 never fires at all, and neither does a timer with no
--     OnTimer assigned.
--   * KillTimer/CreateTimer means any change to Enabled, Interval or OnTimer
--     restarts the countdown from that moment.
--
-- Enabled = false is a *pause* (KillTimer leaves the component alive, so
-- re-enabling recreates the underlying timer); only destroy() ends it. The two
-- used to be the same thing here, which quietly deleted any timer created via
-- `createTimer(owner, false)` if a tick landed before it was enabled.
function createTimer(owner, enabled)
  local s = { Interval = 1000, Enabled = true, OnTimer = nil,
              __deadline = nil, __destroyed = false }
  if enabled == false then s.Enabled = false end

  local function update()          -- TCustomTimer.UpdateTimer
    if s.Enabled and (tonumber(s.Interval) or 0) > 0 and s.OnTimer and not s.__destroyed then
      s.__deadline = __htt_now() + s.Interval
    else
      s.__deadline = nil
    end
  end

  local t = {}
  local methods = {}
  methods.destroy = function() s.__destroyed = true; s.__deadline = nil end
  methods.Destroy = methods.destroy
  methods.free = methods.destroy
  methods.Free = methods.destroy
  -- Callable as t.setX(v) and t:setX(v) alike, as the dual registration allows.
  methods.setOnTimer  = function(a, b) s.OnTimer  = (type(a) == 'function') and a or b; update() end
  methods.setInterval = function(a, b) s.Interval = tonumber(a) and a or b; update() end
  methods.setEnabled  = function(a, b) s.Enabled  = (type(a) == 'boolean') and a or b; update() end

  setmetatable(t, {
    __index = function(_, k)
      if methods[k] ~= nil then return methods[k] end
      return s[k]
    end,
    __newindex = function(_, k, v)
      s[k] = v
      if k == 'Enabled' or k == 'Interval' or k == 'OnTimer' then update() end
    end,
  })

  -- The handler receives the object the table holds, not the backing store:
  -- `timer.OnTimer = function(timer) ... timer.Destroy() end` is the standard
  -- shape, and RDR2's onOpenProcess uses exactly it.
  s.__proxy = t

  __htt_timers[#__htt_timers + 1] = s
  update()
  return t
end

-- Inside a thread this yields; elsewhere it really sleeps.
function sleep(ms)
  ms = tonumber(ms) or 0
  if coroutine.isyieldable() then return coroutine.yield(ms) end
  __htt_realsleep(ms)
end
Sleep = sleep

function checkSynchronize() end
function synchronize(f, ...) if type(f) == 'function' then return f(...) end end

function __htt_hasWork()
  return #__htt_threads > 0 or #__htt_timers > 0
end

function __htt_tick(now)
  for i = #__htt_threads, 1, -1 do
    local t = __htt_threads[i]
    if t.obj.Terminated or coroutine.status(t.co) == 'dead' then
      table.remove(__htt_threads, i)
    elseif now >= t.wake then
      local ok, res = coroutine.resume(t.co)
      if not ok then
        __htt_log('thread error: ' .. tostring(res))
        table.remove(__htt_threads, i)
      elseif coroutine.status(t.co) == 'dead' then
        table.remove(__htt_threads, i)
      else
        t.wake = now + (tonumber(res) or 0)
      end
    end
  end

  for i = #__htt_timers, 1, -1 do
    local t = __htt_timers[i]
    if t.__destroyed then
      table.remove(__htt_timers, i)
    elseif t.__deadline and now >= t.__deadline then
      -- Periodic, like the widgetset timer: rearm before firing, so a handler
      -- that disables or destroys the timer still wins.
      t.__deadline = now + t.Interval
      local ok, err = pcall(t.OnTimer, t.__proxy)
      if not ok then __htt_log('timer error: ' .. tostring(err)) end
    end
  end
end
"#;

/// the AOB scans return a TStringList of hex address strings; tables index it
/// with `[0]` and call `.destroy()`. Mirror that shape.
fn scan_result(lua: &Lua, found: Option<usize>) -> mlua::Result<Value> {
    let Some(addr) = found else { return Ok(Value::Nil) };
    let t = lua.create_table()?;
    let hex = format!("{:X}", addr);
    t.set(0, hex.clone())?;
    t.set("Count", 1)?;
    let noop = lua.create_function(|_, _: Variadic<Value>| Ok(()))?;
    t.set("destroy", noop.clone())?;
    t.set("Destroy", noop.clone())?;
    t.set("free", noop)?;
    t.set(
        "getString",
        lua.create_function(move |_, _: Variadic<Value>| Ok(hex.clone()))?,
    )?;
    Ok(Value::Table(t))
}

/// Builds the Lua-side view of a memory record.
///
/// Reads come from the snapshot; assigning `Active`/`Value` routes through
/// `__newindex` into the deferred action queue.
fn build_record_table(lua: &Lua, r: &RecordView) -> mlua::Result<Value> {
    let t = lua.create_table()?;
    t.set("ID", r.id as i64)?;
    t.set("Id", r.id as i64)?;
    t.set("Description", r.description.clone())?;
    t.set("__id", r.id as i64)?;
    t.set("__active", r.active)?;
    t.set("Count", r.child_ids.len() as i64)?;

    // Children are exposed as a 0-based `Child` array property.
    //
    // Built lazily: a record tree several levels deep would otherwise expand
    // every descendant on every lookup, which is exponential on a table like
    // RDR2's (2390 records).
    let ids = lua.create_table()?;
    for (i, cid) in r.child_ids.iter().enumerate() {
        ids.set(i as i64, *cid as i64)?;
    }
    let children = lua.create_table()?;
    let child_meta = lua.create_table()?;
    child_meta.set(
        "__index",
        lua.create_function(move |lua, (this, key): (Table, Value)| {
            let idx = match key {
                Value::Integer(i) => i,
                Value::Number(n) => n as i64,
                _ => return Ok(Value::Nil),
            };
            let ids: Table = this.raw_get("__ids")?;
            // Accept both 0-based (table idiom) and 1-based (Lua idiom) indexing.
            let id: Option<i64> = ids.get(idx).ok().or_else(|| ids.get(idx - 1).ok());
            match id {
                Some(id) => {
                    let f: mlua::Function = lua.globals().get("__htt_recordById")?;
                    f.call(id)
                }
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    children.raw_set("__ids", ids)?;
    children.set_metatable(Some(child_meta));
    t.set("Child", children.clone())?;
    t.set("child", children)?;

    let meta = lua.create_table()?;
    let index = lua.create_function(|lua, (this, key): (Table, String)| {
        let id: i64 = this.raw_get("__id")?;
        match key.as_str() {
            "Active" | "active" => Ok(Value::Boolean(this.raw_get("__active")?)),
            "Address" | "address" => {
                let f: mlua::Function = lua.globals().get("__htt_getRecordAddress")?;
                f.call(id)
            }
            // A memory record carries a large surface (hotkeys, colours,
            // dropdown options, ...). Anything not modelled here yields a
            // permissive dummy so a chained call does not abort the script.
            _ => dummy_object(lua),
        }
    })?;
    let newindex = lua.create_function(|lua, (this, key, val): (Table, String, Value)| {
        let id: i64 = this.raw_get("__id")?;
        let g = lua.globals();
        match key.as_str() {
            "Active" | "active" => {
                let on = matches!(val, Value::Boolean(true))
                    || matches!(val, Value::Integer(i) if i != 0);
                this.raw_set("__active", on)?;
                let f: mlua::Function = g.get("__htt_setRecordActive")?;
                f.call::<()>((id, on))?;
            }
            "Value" | "value" => {
                let f: mlua::Function = g.get("__htt_setRecordValue")?;
                f.call::<()>((id, fmt_value(&val)))?;
            }
            _ => {
                this.raw_set(key, val)?;
            }
        }
        Ok(())
    })?;
    meta.set("__index", index)?;
    meta.set("__newindex", newindex)?;
    t.set_metatable(Some(meta));

    Ok(Value::Table(t))
}

/// The one clock the scheduler uses, shared by `tick`, `getTickCount` and
/// `__htt_now` so timer deadlines are comparable with the `now` a tick supplies.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A permissive stand-in for the GUI objects.
///
/// Headless tables still do things like `getMainForm().Splitter1.Visible=false`.
/// Any field access yields another dummy and any call returns one, so GUI
/// chains are harmless instead of raising.
fn dummy_object(lua: &Lua) -> mlua::Result<Value> {
    let t = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", lua.create_function(|lua, _: (Table, Value)| dummy_object(lua))?)?;
    meta.set("__call", lua.create_function(|lua, _: Variadic<Value>| dummy_object(lua))?)?;
    meta.set("__newindex", lua.create_function(|_, _: (Table, Value, Value)| Ok(()))?)?;
    t.set_metatable(Some(meta));
    Ok(Value::Table(t))
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.to_str().map(|x| x.to_string()).unwrap_or_default(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => "nil".into(),
        _ => format!("{:?}", v),
    }
}

fn last_integer(args: &Variadic<Value>) -> Option<i64> {
    args.iter().rev().find_map(|v| match v {
        Value::Integer(i) => Some(*i),
        Value::Number(f) => Some(*f as i64),
        Value::String(s) => s.to_str().ok().and_then(|x| x.parse::<i64>().ok()),
        _ => None,
    })
}

/// Accepts a numeric address or an address expression such as
/// `"[coord2]+70"` or `"game.exe+1A2B"`.
fn resolve_addr(
    v: &Value,
    process: Option<&GameProcess>,
    symbols: &SymbolHandler,
) -> mlua::Result<usize> {
    match v {
        Value::Integer(i) => Ok(*i as usize),
        Value::Number(f) => Ok(*f as usize),
        Value::String(s) => {
            let expr = s.to_str()?;
            evaluate_address(&expr, process, symbols).map_err(mlua::Error::external)
        }
        _ => Err(mlua::Error::external("expected an address")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: &str, records: Vec<RecordView>) -> (Option<String>, Vec<LuaAction>) {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records, actions: vec![] };
        let out = e.run_block(code, None, &mut ctx).unwrap();
        (out, ctx.actions)
    }

    #[test]
    fn block_returning_a_string_produces_assembler_source() {
        let (out, _) = run("return 'mov rax,1\\nnop'", vec![]);
        assert_eq!(out.as_deref(), Some("mov rax,1\nnop"));
    }

    #[test]
    fn block_returning_nothing_contributes_no_source() {
        let (out, _) = run("local x = 1 + 1", vec![]);
        assert_eq!(out, None);
    }

    #[test]
    fn symbols_registered_from_lua_reach_the_symbol_table() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        {
            let mut ctx =
                LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };
            e.run_block("registerSymbol('luaSym', 0x1234)", None, &mut ctx).unwrap();
        }
        assert_eq!(syms.get_address_from_name("luaSym", None).unwrap(), 0x1234);
    }

    fn rec(id: u32, desc: &str) -> RecordView {
        RecordView {
            id,
            description: desc.into(),
            address_expr: String::new(),
            active: false,
            is_aa: true,
            child_ids: Vec::new(),
        }
    }

    #[test]
    fn record_activation_is_queued_not_applied_inline() {
        let (_, actions) = run(
            "getAddressList().getMemoryRecordByID(42).Active = true",
            vec![rec(42, "Infinite Health")],
        );
        assert_eq!(actions, vec![LuaAction::SetActive { id: 42, active: true }]);
    }

    #[test]
    fn colon_call_syntax_also_works() {
        // Tables use both `al.getMemoryRecordByID(id)` and `al:getMemoryRecordByID(id)`.
        let (out, _) = run(
            "local al = getAddressList()
             return tostring(al:getMemoryRecordByID(5).ID)",
            vec![rec(5, "x")],
        );
        assert_eq!(out.as_deref(), Some("5"));
    }

    /// The scan classes, as the `aobScanEx` helper in Otis_Inf / SunBeam
    /// tables uses them. The shapes match the canonical ones: a found list hands
    /// back a *hex string* (via `inttohex(...,8)`), and both list types
    /// index from 0 and expose `Count`.
    #[test]
    fn scan_classes_match_expected_shapes() {
        // Constants, the canonical values.
        let (out, _) = run(
            "return tostring(soExactValue)..','..tostring(vtByteArray)..','..tostring(fsmNotAligned)",
            vec![],
        );
        assert_eq!(out.as_deref(), Some("1,8,0"));

        // A string list is 0-indexed, counts, and survives tonumber(x, 16) -
        // which is exactly how a table reads an address back out of a scan.
        let (out, _) = run(
            "local l = createStringlist()
             l.add('0000DEAD') l.add('0000BEEF')
             return tostring(l.Count)..','..l[0]..','..tostring(tonumber(l[1], 16))",
            vec![],
        );
        assert_eq!(out.as_deref(), Some("2,0000DEAD,48879"));

        // A found list over a scan that never ran is empty rather than an
        // error: the helper checks `Count < 1` before it indexes.
        let (out, _) = run(
            "local ms = createMemScan()
             local fl = createFoundList(ms)
             fl.initialize()
             return tostring(fl.getCount())..','..tostring(fl.Count)..','..tostring(fl.getAddress(0))",
            vec![],
        );
        assert_eq!(out.as_deref(), Some("0,0,nil"));

        // An unimplemented scan type must say so rather than silently finding
        // nothing, which would read as "the signature is gone".
        let (out, _) = run(
            "local ms = createMemScan()
             local ok, err = pcall(function()
               ms.firstScan(soExactValue, vtDword, nil, '123', nil, 0, 16)
             end)
             return tostring(ok)..','..tostring(tostring(err):find('only soExactValue') ~= nil)",
            vec![],
        );
        assert_eq!(out.as_deref(), Some("false,true"));

        // A table calls these unconditionally, so they must exist as no-ops.
        let (out, _) = run(
            "local ms = createMemScan()
             ms.waitTillDone()
             local fl = createFoundList(ms)
             fl.destroy() ms.destroy()
             return 'survived'",
            vec![],
        );
        assert_eq!(out.as_deref(), Some("survived"));
    }

    /// The scan helpers need a live process. Without one they must fail
    /// cleanly, because `--check-lua` and the analysis modes run detached.
    #[test]
    fn scan_helpers_without_a_process_fail_cleanly() {
        let (out, _) = run(
            "return tostring(pcall(function() return getModuleSize('game.exe') end))",
            vec![],
        );
        assert_eq!(out.as_deref(), Some("false"));
    }

    #[test]
    fn lookup_by_description_matches_ce_behaviour() {
        let (out, _) = run(
            "local r = getAddressList().getMemoryRecordByDescription('Godmode')
             if r == nil then return 'MISSING' end
             return tostring(r.ID)",
            vec![rec(7, "Godmode")],
        );
        assert_eq!(out.as_deref(), Some("7"));
    }

    #[test]
    fn missing_record_returns_nil_rather_than_erroring() {
        let (out, _) = run(
            "if getAddressList().getMemoryRecordByID(999) == nil then return 'nil-ok' end
             return 'unexpected'",
            vec![rec(1, "a")],
        );
        assert_eq!(out.as_deref(), Some("nil-ok"));
    }

    #[test]
    fn gui_calls_are_harmless_in_headless_mode() {
        // Taken from the RDR2 table's compact-view script.
        let (_, _) = run(
            "getMainForm().Splitter1.Visible = true
             local m = createMenuItem(getMainForm())
             m.Caption = 'x'
             m.OnClick = function() end
             return nil",
            vec![],
        );
    }

    #[test]
    fn memrec_argument_is_passed_to_the_block() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };
        let me = rec(99, "Self");
        let out = e.run_block("return tostring(memrec.ID)", Some(&me), &mut ctx).unwrap();
        assert_eq!(out.as_deref(), Some("99"));
    }

    #[test]
    fn memory_access_without_a_process_errors_cleanly() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };
        assert!(e.run_block("return tostring(readInteger(0x1000))", None, &mut ctx).is_err());
    }

    #[test]
    fn retained_functions_are_dead_after_the_call_returns() {
        // A script that stashes an API function must not reach freed state later.
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        {
            let mut ctx =
                LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };
            e.run_block("stashed = registerSymbol", None, &mut ctx).unwrap();
        }
        let mut syms2 = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms2, records: vec![], actions: vec![] };
        // Calling the stale handle from a *new* call is fine (bridge re-armed),
        // but the stale Rc from the previous call was invalidated.
        let out = e.run_block(
            "local ok = pcall(function() stashed('x', 1) end) return tostring(ok)",
            None,
            &mut ctx,
        );
        assert!(out.is_ok());
    }

    /// The standard trainer-thread shape must actually make progress across
    /// ticks, and must stop when terminated.
    #[test]
    fn create_thread_runs_across_ticks_and_terminates() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };

        e.run_block(
            "ticks = 0
             worker = createThread(function(t)
               while not t.Terminated do
                 ticks = ticks + 1
                 sleep(0)
               end
             end)",
            None,
            &mut ctx,
        )
        .unwrap();

        // Not started until the runtime ticks.
        let count = |e: &LuaEngine, ctx: &mut LuaCtx| -> i64 {
            e.run_block("return tostring(ticks)", None, ctx)
                .unwrap()
                .unwrap()
                .parse()
                .unwrap()
        };
        assert_eq!(count(&e, &mut ctx), 0);

        for _ in 0..3 {
            e.tick(&mut ctx).unwrap();
        }
        let after = count(&e, &mut ctx);
        assert!(after >= 3, "thread did not progress: {}", after);

        // Terminating stops further progress and drops the thread.
        e.run_block("worker.terminate()", None, &mut ctx).unwrap();
        e.tick(&mut ctx).unwrap();
        let stopped = count(&e, &mut ctx);
        e.tick(&mut ctx).unwrap();
        assert_eq!(count(&e, &mut ctx), stopped);
    }

    /// A thread body that never yields must not hang the runtime.
    #[test]
    fn a_finite_thread_body_completes_and_is_reaped() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };

        e.run_block("done = false createThread(function(t) done = true end)", None, &mut ctx)
            .unwrap();
        e.tick(&mut ctx).unwrap();

        let out = e.run_block("return tostring(done)", None, &mut ctx).unwrap();
        assert_eq!(out.as_deref(), Some("true"));
        // Reaped, so no further work is scheduled.
        let work = e
            .run_block("return tostring(#__htt_threads)", None, &mut ctx)
            .unwrap();
        assert_eq!(work.as_deref(), Some("0"));
    }

    /// Reads `fired` out of a scheduler test's Lua state.
    fn fired(e: &LuaEngine, ctx: &mut LuaCtx<'_>) -> i64 {
        e.run_block("return tostring(fired)", None, ctx)
            .unwrap()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// `createTimer` builds an LCL `TTimer`, whose update path is
    ///
    /// ```text
    /// kill any existing timer, then - if enabled, interval > 0 and a
    /// handler is assigned - schedule a new one Interval ms out
    /// ```
    ///
    /// so the handler is scheduled `Interval` ms out and never runs on enable.
    /// This engine used to fire every timer once on the first tick regardless,
    /// which threw away the delay RDR2's `onOpenProcess` deliberately asks for.
    #[test]
    fn a_timer_waits_a_full_interval_before_its_first_fire() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };

        e.run_block(
            "fired = 0
             tmr = createTimer(nil, false)
             tmr.Interval = 250
             tmr.OnTimer = function() fired = fired + 1 end
             tmr.Enabled = true",
            None,
            &mut ctx,
        )
        .unwrap();

        // Immediately, and well inside the interval: nothing.
        e.tick(&mut ctx).unwrap();
        assert_eq!(fired(&e, &mut ctx), 0, "fired before its interval elapsed");
        std::thread::sleep(std::time::Duration::from_millis(120));
        e.tick(&mut ctx).unwrap();
        assert_eq!(fired(&e, &mut ctx), 0, "fired before its interval elapsed");

        // Past it: exactly once, then periodically.
        std::thread::sleep(std::time::Duration::from_millis(180));
        e.tick(&mut ctx).unwrap();
        assert_eq!(fired(&e, &mut ctx), 1);
    }

    /// `UpdateTimer` refuses to create the underlying timer unless
    /// `FInterval > 0`, and `TCustomTimer.Timer` re-checks it before dispatch.
    #[test]
    fn a_zero_interval_timer_never_fires() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };

        e.run_block(
            "fired = 0
             tmr = createTimer(nil, true)
             tmr.Interval = 0
             tmr.OnTimer = function() fired = fired + 1 end",
            None,
            &mut ctx,
        )
        .unwrap();

        for _ in 0..3 {
            e.tick(&mut ctx).unwrap();
        }
        assert_eq!(fired(&e, &mut ctx), 0);
    }

    /// `Enabled := false` calls `KillTimer` but leaves the component alive, so
    /// re-enabling recreates the timer. This engine used to delete a disabled
    /// timer outright, which silently destroyed anything built by
    /// `createTimer(owner, false)` if a tick landed before it was enabled -
    /// exactly the shape RDR2's `onOpenProcess` uses.
    #[test]
    fn disabling_a_timer_pauses_it_and_destroying_it_ends_it() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };

        e.run_block(
            "fired = 0
             tmr = createTimer(nil, false)
             tmr.Interval = 50
             tmr.OnTimer = function() fired = fired + 1 end",
            None,
            &mut ctx,
        )
        .unwrap();

        // A tick while disabled must not discard it.
        std::thread::sleep(std::time::Duration::from_millis(70));
        e.tick(&mut ctx).unwrap();
        assert_eq!(fired(&e, &mut ctx), 0);

        e.run_block("tmr.Enabled = true", None, &mut ctx).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(70));
        e.tick(&mut ctx).unwrap();
        assert_eq!(fired(&e, &mut ctx), 1, "re-enabling must revive the timer");

        // Destroy is the only thing that ends it.
        e.run_block("tmr.destroy()", None, &mut ctx).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(70));
        e.tick(&mut ctx).unwrap();
        assert_eq!(fired(&e, &mut ctx), 1);
    }

    /// The handler receives the timer object the table holds - RDR2's
    /// `onOpenProcess` calls `timer.Destroy()` on its argument.
    #[test]
    fn a_timer_handler_can_destroy_itself_through_its_argument() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };

        e.run_block(
            "fired = 0
             tmr = createTimer(nil, false)
             tmr.Interval = 40
             tmr.OnTimer = function(t) fired = fired + 1; t.Destroy() end
             tmr.Enabled = true",
            None,
            &mut ctx,
        )
        .unwrap();

        for _ in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            e.tick(&mut ctx).unwrap();
        }
        assert_eq!(fired(&e, &mut ctx), 1, "handler destroyed it, so it must fire once");
    }

    #[test]
    fn a_thread_can_toggle_records_via_the_action_queue() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx {
            process: None,
            symbols: &mut syms,
            records: vec![rec(11, "Godmode")],
            actions: vec![],
        };

        e.run_block(
            "createThread(function(t)
               getAddressList().getMemoryRecordByID(11).Active = true
               t.terminate()
             end)",
            None,
            &mut ctx,
        )
        .unwrap();
        assert!(ctx.actions.is_empty());

        e.tick(&mut ctx).unwrap();
        assert_eq!(ctx.actions, vec![LuaAction::SetActive { id: 11, active: true }]);
    }

    #[test]
    fn tick_is_cheap_when_nothing_is_scheduled() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };
        // No prelude loaded yet, and no work afterwards.
        e.tick(&mut ctx).unwrap();
        e.run_block("local x = 1", None, &mut ctx).unwrap();
        e.tick(&mut ctx).unwrap();
    }

    #[test]
    fn errors_in_a_thread_body_do_not_abort_the_runtime() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };

        e.run_block("createThread(function(t) error('boom') end)", None, &mut ctx)
            .unwrap();
        e.tick(&mut ctx).unwrap();
        // Faulted thread is dropped, and later ticks still work.
        e.tick(&mut ctx).unwrap();
        let out = e
            .run_block("return tostring(#__htt_threads)", None, &mut ctx)
            .unwrap();
        assert_eq!(out.as_deref(), Some("0"));
    }

    #[test]
    fn lua_call_expression_runs_for_side_effects() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };
        e.run_call("registerSymbol('fromCall', 0x99)", &mut ctx).unwrap();
        assert_eq!(syms.get_address_from_name("fromCall", None).unwrap(), 0x99);
    }

    #[test]
    fn a_missing_callback_is_not_an_error() {
        // Guarded with `lua_isfunction` before calling,
        // so most tables - which define none of these - cost nothing.
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };
        assert!(!e.call_callback("onOpenProcess", vec![], &mut ctx));
    }

    #[test]
    fn an_error_inside_a_callback_does_not_escape() {
        // Invoked through `lua_pcall`. A table's broken
        // callback must not take the trainer down.
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: vec![], actions: vec![] };
        e.run_block("function onOpenProcess() error('boom') end", None, &mut ctx).unwrap();
        assert!(e.call_callback("onOpenProcess", vec![], &mut ctx));
    }

    /// The RDR2 table's whole foundation hangs off `onOpenProcess`: it arms a
    /// timer that activates record 397906, the root group, whose
    /// `moActivateChildrenAsWell` then brings up all nine "BASE" scripts -
    /// `Main CodeBase` among them. Before this callback was wired up, none of
    /// that happened and the table had to be driven by hand.
    #[test]
    fn rdr2_onopenprocess_activates_the_root_group() {
        use crate::ct_types::{CheatEntry, VariableType};

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rdr2.CT");
        let table = crate::ct_parser::parse_file(path).unwrap();

        let mut flat: Vec<CheatEntry> = Vec::new();
        fn walk(entries: &[CheatEntry], out: &mut Vec<CheatEntry>) {
            for e in entries {
                out.push(e.clone());
                walk(&e.children, out);
            }
        }
        walk(&table.entries, &mut flat);
        let views: Vec<RecordView> = flat
            .iter()
            .map(|e| RecordView {
                id: e.id,
                description: e.description.clone(),
                address_expr: e.address.clone().unwrap_or_default(),
                active: false,
                is_aa: e.var_type == VariableType::AutoAssembler,
                child_ids: e.children.iter().map(|c| c.id).collect(),
            })
            .collect();

        let src = match table.lua_script.expect("rdr2.CT has a <LuaScript>") {
            crate::ct_types::LuaScript::Single(s) => s,
            crate::ct_types::LuaScript::MultiTab(v) => {
                v.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("\n")
            }
        };

        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx { process: None, symbols: &mut syms, records: views, actions: vec![] };
        e.set_global_string("TrainerOrigin", "rdr2").unwrap();
        e.run_block(&src, None, &mut ctx).expect("table script must run clean");

        // The table declares its target the same way the generated trainers do.
        assert!(ctx
            .actions
            .iter()
            .any(|a| matches!(a, LuaAction::SetAutoAttach { process } if process == "RDR2.exe")));

        ctx.actions.clear();
        assert!(e.call_callback("onOpenProcess", vec![], &mut ctx), "onOpenProcess must exist");

        // The callback only arms a timer; the activation lands on a later tick.
        for _ in 0..40 {
            if !ctx.actions.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            e.tick(&mut ctx).unwrap();
        }
        assert_eq!(ctx.actions, vec![LuaAction::SetActive { id: 397906, active: true }]);
    }

    /// `<Options .../>` is written self-closing, which quick-xml reports as
    /// `Event::Empty`. Parsing only `Event::Start` dropped every record option
    /// in every real table, which made `moActivateChildrenAsWell` false
    /// everywhere and left the cascade in `set_active` unreachable.
    #[test]
    fn self_closing_options_elements_are_read() {
        let table =
            crate::ct_parser::parse_file(concat!(env!("CARGO_MANIFEST_DIR"), "/../rdr2.CT")).unwrap();
        let root = table
            .entries
            .iter()
            .find(|e| e.id == 397906)
            .expect("the RDR2 root group");
        assert!(root.options.activate_children_as_well, "cascade flag was dropped");
        assert!(root.options.deactivate_children_as_well);
        assert!(root.options.always_hide_children);
        assert_eq!(root.children.len(), 9, "the nine BASE scripts");
        let main_codebase = root.children.iter().find(|e| e.id == 54).unwrap();
        assert!(main_codebase.options.activate_children_as_well);
    }

    /// the `$name` reads a global, falls back to
    /// evaluating it as an expression, and then branches on the *type* of the
    /// result: userdata and numbers become an address directly, strings get
    /// resolved as a symbol in their own right.
    ///
    /// This covers the string and expression arms, which is what `$process`
    /// needs — `TProcessHandler.Open` publishes the target's module name as the
    /// global `process`, and that is the form
    /// every table in the corpus uses.
    ///
    /// The numeric arm is *not* equivalent to the original and this pins the
    /// difference rather than pretending otherwise: it turns a numeric global into that
    /// integer's address, whereas reading it back as a string yields its
    /// decimal text, which any later parse would read as hex. No table in the
    /// corpus writes `$someNumericGlobal`, so this is a documented gap, not a
    /// live bug — but a caller must not assume the numeric case works.
    #[test]
    fn dollar_name_reads_globals_the_way_ce_does() {
        let e = LuaEngine::new();

        e.set_global_string("process", "RDR2.exe").unwrap();
        assert_eq!(e.global_string("process").as_deref(), Some("RDR2.exe"));

        // Missing global, and not a valid expression either.
        assert_eq!(e.global_string("nope_not_here"), None);

        // The expression fallback: not a global, but evaluable.
        e.lua
            .load("function whereAmI() return 'RDR2.exe' end")
            .exec()
            .unwrap();
        assert_eq!(e.global_string("whereAmI()").as_deref(), Some("RDR2.exe"));

        // Numeric globals: whatever comes back, it must not be mistaken for a
        // module name. Recorded so the gap above stays visible.
        e.lua.globals().set("someAddr", 0x1234i64).unwrap();
        let numeric = e.global_string("someAddr");
        assert_ne!(
            numeric.as_deref(),
            Some("RDR2.exe"),
            "a numeric global must never masquerade as a module"
        );
    }

    /// A nested `autoAssemble()` must run on the *caller's* Lua state.
    ///
    /// There is exactly one state, and tables rely on it. RDR2's BlipPTR does
    /// `aobscanmodule(BlipPTR,$process,...)` from inside
    /// `autoAssemble([[...]])`; `$process` reads the global that
    /// `TProcessHandler.Open` publishes. This
    /// used to build a fresh `LuaEngine` for the nested script, so that global
    /// — and every helper the table had defined — was missing, the module
    /// argument could not resolve, and the scan failed.
    ///
    /// Checked through globals rather than by running a scan, because the
    /// visibility of the outer state is the whole property; a scan needs a live
    /// process and would test the wrong thing.
    #[test]
    fn a_nested_script_sees_the_outer_lua_state() {
        let e = LuaEngine::new();
        let mut syms = SymbolHandler::new();
        let mut ctx = LuaCtx {
            process: None,
            symbols: &mut syms,
            records: Vec::new(),
            actions: Vec::new(),
        };

        // Publish `process` exactly as on_process_opened does, then have Lua
        // read it back from inside a call that goes through the frame stack.
        e.set_global_string("process", "RDR2.exe").unwrap();
        let seen = e
            .run_block("return 'saw:' .. tostring(process)", None, &mut ctx)
            .expect("block ran");
        assert_eq!(
            seen.as_deref(),
            Some("saw:RDR2.exe"),
            "the global set outside the block was not visible inside it"
        );

        // And a helper defined by one block is visible to the next, which is
        // the same single-state property from the other direction.
        e.run_block("function tableHelper() return 'shared' end", None, &mut ctx).ok();
        let again = e
            .run_block("return tableHelper()", None, &mut ctx)
            .expect("second block ran");
        assert_eq!(again.as_deref(), Some("shared"));
    }

}

// ---------------------------------------------------------------------------
// Win32 helpers backing the window/input part of the table Lua API
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn find_window(title: Option<&str>) -> isize {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    let Some(t) = title else { return 0 };
    unsafe {
        let h = HSTRING::from(t);
        FindWindowW(None, &h).map(|w| w.0 as isize).unwrap_or(0)
    }
}

#[cfg(windows)]
fn get_foreground_window() -> isize {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow().0 as isize }
}

#[cfg(windows)]
fn set_foreground_window(h: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    if h == 0 {
        return;
    }
    unsafe {
        let _ = SetForegroundWindow(HWND(h as *mut _));
    }
}

#[cfg(windows)]
fn send_mouse_event(flags: u32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{mouse_event, MOUSE_EVENT_FLAGS};
    unsafe { mouse_event(MOUSE_EVENT_FLAGS(flags), 0, 0, 0, 0) };
}

#[cfg(windows)]
fn shell_execute(cmd: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    if cmd.is_empty() {
        return;
    }
    unsafe {
        let op = HSTRING::from("open");
        let file = HSTRING::from(cmd);
        let _ = ShellExecuteW(None, &op, &file, None, None, SW_SHOWNORMAL);
    }
}

// This is a headless build with no window-system integration by design (see
// CLAUDE.md and LINUX-WINDOWS-PARITY-PLAN.md §2.1/§3.3) - under Wayland there
// is no protocol for querying or activating another client's window anyway,
// so a partial X11-only implementation would work on some sessions and not
// others for reasons a table author cannot see. Rather than a plausible-looking
// value (a real bug here masqueraded as "game not running" for a long time),
// each function logs its unavailability once and returns an honest,
// documented no-op.

#[cfg(not(windows))]
fn warn_unavailable_once(once: &std::sync::Once, api: &str, effect: &str) {
    once.call_once(|| {
        warn!(
            "{} is not available in this build of the runtime (it has no \
             window-system integration; a shipped trainer is the Windows build \
             and runs under Proton, where this API works) - {}",
            api, effect
        );
    });
}

#[cfg(not(windows))]
fn find_window(_: Option<&str>) -> isize {
    static ONCE: std::sync::Once = std::sync::Once::new();
    warn_unavailable_once(&ONCE, "findWindow", "it will always report 'not found' (0)");
    0
}
#[cfg(not(windows))]
fn get_foreground_window() -> isize {
    static ONCE: std::sync::Once = std::sync::Once::new();
    warn_unavailable_once(&ONCE, "getForegroundWindow", "it will always return 0");
    0
}
#[cfg(not(windows))]
fn set_foreground_window(_: isize) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    warn_unavailable_once(&ONCE, "setForegroundWindow", "it has no effect");
}
#[cfg(not(windows))]
fn send_mouse_event(_: u32) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    warn_unavailable_once(&ONCE, "mouse_event", "it has no effect");
}

/// Mirrors `ShellExecuteW(..., "open", cmd, ...)`: `xdg-open` resolves a URL,
/// document or executable the same way, by desktop MIME association, rather
/// than adding a GUI dependency to this engine.
#[cfg(not(windows))]
fn shell_execute(cmd: &str) {
    if cmd.is_empty() {
        return;
    }
    if let Err(e) = std::process::Command::new("xdg-open").arg(cmd).spawn() {
        warn!("shellExecute('{}') failed to launch xdg-open: {}", cmd, e);
    }
}
