use crate::address_parser::evaluate_address;
use crate::auto_assembler::AutoAssemblerScript;
use crate::ct_types::{CheatEntry, EntryOptions, VariableType as CtVarType};
use crate::lua_engine::{ExecEnv, RecordView};
use crate::process::GameProcess;
use crate::symbol_handler::SymbolHandler;
use anyhow::{bail, Result};
use tracing::warn;

/// Fires one of the memory-record Lua callbacks.
///
/// Both are observers - `onMemRecPreExecute`'s result is discarded and
/// `onMemRecPostExecute` returns nothing - so neither can change what happens
/// here, and a table that defines neither (the overwhelming majority) pays
/// only a global lookup.
fn notify(
    env: &mut ExecEnv<'_>,
    process: &mut GameProcess,
    name: &str,
    rec: &RecordView,
    state: bool,
) {
    let mut ctx = crate::lua_engine::LuaCtx {
        process: Some(process),
        symbols: env.symbols,
        records: env.record_views.to_vec(),
        actions: Vec::new(),
    };
    env.lua.call_record_callback(name, rec, vec![mlua::Value::Boolean(state)], &mut ctx);
    // A callback may itself ask for a record change; queue it the way a
    // `{$lua}` block's requests are queued rather than re-entering the AA
    // engine from inside an activation.
    env.actions.append(&mut ctx.actions);
}

/// `onMemRecPostExecute(memrec, wantedstate, wantedstate==actualstate)`.
fn notify_result(
    env: &mut ExecEnv<'_>,
    process: &mut GameProcess,
    rec: &RecordView,
    wanted: bool,
    succeeded: bool,
) {
    let mut ctx = crate::lua_engine::LuaCtx {
        process: Some(process),
        symbols: env.symbols,
        records: env.record_views.to_vec(),
        actions: Vec::new(),
    };
    env.lua.call_record_callback(
        "onMemRecPostExecute",
        rec,
        vec![mlua::Value::Boolean(wanted), mlua::Value::Boolean(succeeded)],
        &mut ctx,
    );
    env.actions.append(&mut ctx.actions);
}

impl From<&CtVarType> for VariableType {
    fn from(t: &CtVarType) -> Self {
        match t {
            CtVarType::Byte => VariableType::Byte,
            CtVarType::Word => VariableType::Word,
            CtVarType::Dword => VariableType::Dword,
            CtVarType::Qword => VariableType::Qword,
            CtVarType::Float => VariableType::Float,
            CtVarType::Double => VariableType::Double,
            CtVarType::String => VariableType::String,
            CtVarType::UnicodeString => VariableType::UnicodeString,
            CtVarType::ByteArray => VariableType::ByteArray,
            CtVarType::Binary => VariableType::Binary,
            CtVarType::AutoAssembler => VariableType::AutoAssembler,
            CtVarType::Pointer => VariableType::Pointer,
            CtVarType::Custom(s) => VariableType::Custom(s.clone()),
            CtVarType::Grouped => VariableType::Grouped,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VariableType {
    Byte,
    Word,
    Dword,
    Qword,
    Float,
    Double,
    String,
    UnicodeString,
    ByteArray,
    Binary,
    AutoAssembler,
    Pointer,
    Custom(String),
    Grouped,
}

#[derive(Debug, Clone)]
pub struct MemoryRecord {
    pub id: u32,
    pub description: String,
    pub var_type: VariableType,
    pub address_expr: String,
    pub offsets: Vec<String>,
    pub active: bool,
    pub frozen_value: Option<String>,
    pub is_group_header: bool,
    pub children: Vec<MemoryRecord>,
    pub aa_script: Option<AutoAssemblerScript>,
    pub options: EntryOptions,
    /// Key bindings declared by the table, driven by `hotkeys.rs`.
    pub hotkeys: Vec<crate::ct_types::Hotkey>,
}

impl MemoryRecord {
    pub fn from_cheat_entry(entry: &CheatEntry) -> MemoryRecord {
        let aa_script = entry.assembler_script.as_ref().map(|s| AutoAssemblerScript::parse(s));
        MemoryRecord {
            id: entry.id,
            description: entry.description.clone(),
            var_type: VariableType::from(&entry.var_type),
            address_expr: entry.address.clone().unwrap_or_default(),
            offsets: entry.offsets.clone(),
            active: false,
            frozen_value: None,
            is_group_header: entry.is_group_header,
            children: entry.children.iter().map(MemoryRecord::from_cheat_entry).collect(),
            aa_script,
            options: entry.options.clone(),
            hotkeys: entry.hotkeys.clone(),
        }
    }

    pub fn resolve_address(&self, process: Option<&GameProcess>, symbols: &SymbolHandler) -> Result<usize> {
        if self.address_expr.is_empty() {
            bail!("Record has no address expression");
        }

        let mut base = evaluate_address(&self.address_expr, process, symbols)?;

        if !self.offsets.is_empty() {
            let p = process.ok_or_else(|| anyhow::anyhow!("Process required to resolve pointer chain"))?;

            // Offsets apply last-to-first (GetRealAddress:
            // `for i := offsetCount-1 downto 0`).
            for off_str in self.offsets.iter().rev() {
                let mut buf = if p.is_64bit { vec![0u8; 8] } else { vec![0u8; 4] };
                p.read_memory(base, &mut buf)
                    .map_err(|_| anyhow::anyhow!("Unreadable pointer at {:#X}", base))?;

                let ptr_val = if p.is_64bit {
                    u64::from_le_bytes(buf.try_into().unwrap()) as i64
                } else {
                    u32::from_le_bytes(buf.try_into().unwrap()) as i64
                };

                // Offsets are hex, and may be negative.
                let t = off_str.trim();
                let (neg, digits) = match t.strip_prefix('-') {
                    Some(rest) => (true, rest.trim()),
                    None => (false, t),
                };
                let digits = digits
                    .strip_prefix("0x")
                    .or_else(|| digits.strip_prefix('$'))
                    .unwrap_or(digits);
                let offset = i64::from_str_radix(digits, 16)
                    .map_err(|_| anyhow::anyhow!("Invalid pointer offset '{}'", off_str))?;

                let delta = if neg { -offset } else { offset };
                base = ptr_val.wrapping_add(delta) as usize;
            }
        }

        Ok(base)
    }

    pub fn read_value(&self, process: &GameProcess, symbols: &SymbolHandler) -> Result<String> {
        if self.is_group_header || self.var_type == VariableType::AutoAssembler {
            return Ok("".to_string());
        }

        let addr = self.resolve_address(Some(process), symbols)?;

        match self.var_type {
            VariableType::Byte => {
                let mut buf = [0u8; 1];
                process.read_memory(addr, &mut buf)?;
                Ok(buf[0].to_string())
            }
            VariableType::Word => {
                let mut buf = [0u8; 2];
                process.read_memory(addr, &mut buf)?;
                Ok(u16::from_le_bytes(buf).to_string())
            }
            VariableType::Dword => {
                let mut buf = [0u8; 4];
                process.read_memory(addr, &mut buf)?;
                Ok(i32::from_le_bytes(buf).to_string())
            }
            VariableType::Qword => {
                let mut buf = [0u8; 8];
                process.read_memory(addr, &mut buf)?;
                Ok(i64::from_le_bytes(buf).to_string())
            }
            VariableType::Float => {
                let mut buf = [0u8; 4];
                process.read_memory(addr, &mut buf)?;
                Ok(f32::from_le_bytes(buf).to_string())
            }
            VariableType::Double => {
                let mut buf = [0u8; 8];
                process.read_memory(addr, &mut buf)?;
                Ok(f64::from_le_bytes(buf).to_string())
            }
            VariableType::String => {
                let mut buf = vec![0u8; 64];
                process.read_memory(addr, &mut buf)?;
                let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                Ok(String::from_utf8_lossy(&buf[..len]).to_string())
            }
            _ => Ok("??".to_string()),
        }
    }

    pub fn write_value(&self, process: &GameProcess, symbols: &SymbolHandler, val_str: &str) -> Result<()> {
        if self.is_group_header || self.var_type == VariableType::AutoAssembler {
            bail!("Cannot set value on group header or Auto Assembler entry");
        }

        let addr = self.resolve_address(Some(process), symbols)?;

        match self.var_type {
            VariableType::Byte => {
                let val: u8 = val_str.parse()?;
                process.write_memory(addr, &[val])
            }
            VariableType::Word => {
                let val: u16 = val_str.parse()?;
                process.write_memory(addr, &val.to_le_bytes())
            }
            VariableType::Dword => {
                let val: i32 = val_str.parse()?;
                process.write_memory(addr, &val.to_le_bytes())
            }
            VariableType::Qword => {
                let val: i64 = val_str.parse()?;
                process.write_memory(addr, &val.to_le_bytes())
            }
            VariableType::Float => {
                let val: f32 = val_str.parse()?;
                process.write_memory(addr, &val.to_le_bytes())
            }
            VariableType::Double => {
                let val: f64 = val_str.parse()?;
                process.write_memory(addr, &val.to_le_bytes())
            }
            VariableType::String => {
                let mut bytes = val_str.as_bytes().to_vec();
                bytes.push(0); // Null terminator
                process.write_memory(addr, &bytes)
            }
            _ => bail!("Unsupported variable type for value writing"),
        }
    }

    pub fn toggle_active(
        &mut self,
        process: &mut GameProcess,
        env: &mut ExecEnv<'_>,
    ) -> Result<bool> {
        let new_state = !self.active;
        self.set_active(new_state, process, env)?;
        Ok(self.active)
    }

    /// The view of this record handed to Lua.
    pub fn to_view(&self) -> RecordView {
        RecordView {
            id: self.id,
            description: self.description.clone(),
            address_expr: self.address_expr.clone(),
            active: self.active,
            is_aa: self.var_type == VariableType::AutoAssembler,
            child_ids: self.children.iter().map(|c| c.id).collect(),
        }
    }

    pub fn set_active(
        &mut self,
        active: bool,
        process: &mut GameProcess,
        env: &mut ExecEnv<'_>,
    ) -> Result<()> {
        if self.active == active {
            return Ok(());
        }

        let view = self.to_view();

        // This fires before anything else happens.
        // Its result is *discarded* there, so it observes and cannot veto -
        // cancellation is `fonactivate`/`fondeactivate`, a different mechanism.
        notify(env, process, "onMemRecPreExecute", &view, active);

        // Children come down before the parent does, and each is waited on -
        // note this sits *above* the
        // record's own disable code, not below it. The order is load-bearing: a
        // parent's [DISABLE] deallocs the memory and unregisters the symbols its
        // children's [DISABLE] still has to write through. Doing it the other
        // way round writes to freed memory.
        if !active && self.options.deactivate_children_as_well {
            for child in &mut self.children {
                if let Err(e) = child.set_active(false, process, env) {
                    warn!("child '{}' failed to deactivate: {}", child.description, e);
                }
            }
        }

        if self.var_type == VariableType::AutoAssembler {
            if let Some(ref mut script) = self.aa_script {
                let r = if active {
                    script.execute_enable(process, env, Some(&view))
                } else {
                    script.execute_disable(process, env, Some(&view))
                };
                if let Err(e) = r {
                    // The landed state is reported, not the wanted one: it
                    // assigns `fActive` from the state actually reached.
                    notify_result(env, process, &view, active, false);
                    return Err(e);
                }
            }
        } else if !self.is_group_header && !self.address_expr.is_empty() {
            // For a value record, the Active checkbox means "freeze". Capture
            // the current value on activation so the freeze loop has something
            // to write back; clear it on deactivation.
            if active {
                if self.frozen_value.is_none() {
                    match self.read_value(process, env.symbols) {
                        Ok(v) => self.frozen_value = Some(v),
                        Err(e) => bail!("cannot freeze '{}': {}", self.description, e),
                    }
                }
            } else {
                self.frozen_value = None;
            }
        }

        self.active = active;

        // Activation cascades the other way round: it happens once processing
        // is done, i.e. only once the
        // parent's own script has finished, because the children address
        // symbols that script registers. RDR2 relies on this - its root group
        // brings up all nine "BASE" scripts this way.
        if active && self.options.activate_children_as_well {
            // Handed to the caller to schedule, not run here - see
            // `ExecEnv::deferred_activations`. This point is reached from
            // `processingDone`, i.e. only once *this* record's script has
            // finished, and each child then activates independently.
            env.deferred_activations
                .extend(self.children.iter().map(|c| (c.id, true)));
        }

        let mut updated = view;
        updated.active = active;
        notify_result(env, process, &updated, active, true);

        Ok(())
    }

    /// Enables every Auto Assembler script in this subtree, returning
    /// `(succeeded, failed)` descriptions. Used to bring up a table's symbol
    /// providers so symbol-addressed records resolve.
    pub fn activate_scripts_recursive(
        &mut self,
        process: &mut GameProcess,
        env: &mut ExecEnv<'_>,
        ok: &mut Vec<String>,
        failed: &mut Vec<(String, String)>,
    ) {
        if self.var_type == VariableType::AutoAssembler && !self.active {
            match self.set_active(true, process, env) {
                Ok(()) => ok.push(self.description.clone()),
                Err(e) => failed.push((self.description.clone(), format!("{:#}", e))),
            }
        }
        for child in &mut self.children {
            child.activate_scripts_recursive(process, env, ok, failed);
        }
    }
}
