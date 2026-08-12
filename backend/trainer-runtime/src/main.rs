mod address_parser;
mod assembler;
mod aob_scanner;
mod auto_assembler;
mod cetrainer;
mod ct_legacy;
mod ct_parser;
mod ct_types;
mod freeze_loop;
mod hotkeys;
mod lua_engine;
mod memory_record;
mod overlay;
mod platform;
mod process;
mod rest_server;
mod symbol_handler;

use anyhow::Result;
use memory_record::MemoryRecord;
use rest_server::{start_rest_server, RuntimeState, SharedState};
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    info!("Starting DeckyTrainers runtime");

    let args: Vec<String> = env::args().collect();

    // `--analyze <table>` reports which of a table's Auto Assembler scripts this
    // engine can assemble, without needing the game running. Useful for checking
    // a table before shipping a trainer for it.
    if args.len() > 2 && args[1] == "--verify-asm" {
        return verify_asm(&args[2]);
    }

    if args.len() > 2 && args[1] == "--check-lua" {
        return check_lua(&args[2]);
    }

    if args.len() > 2 && args[1] == "--analyze" {
        return analyze_table(&args[2], args.get(3).map(|s| s.as_str()));
    }

    let self_exe_name = env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "trainer.exe".to_string());

    // A generated trainer carries its own table appended past its last PE
    // section (see overlay.rs); prefer that over any path argument or on-disk
    // file, so a trainer needs neither.
    let overlay = match overlay::read_self_overlay() {
        Ok(o) => o,
        Err(e) => {
            // A present-but-corrupt overlay is a hard error, not a silent
            // fall-through to "no table" - see overlay.rs's doc comment.
            anyhow::bail!("embedded table is corrupt: {:#}", e);
        }
    };

    let (records, table_filename, table_script) = if let Some(files) = overlay {
        info!("Loading embedded table ({} file(s) in overlay)", files.len());
        write_sibling_defines_lua(&files);

        let cetrainer = files
            .iter()
            .find(|f| f.filename == "HTT_TRAINER.CETRAINER" || f.filename == "CET_TRAINER.CETRAINER");

        match cetrainer {
            Some(f) => match ct_parser::parse_bytes(&f.data) {
                Ok(ct) => {
                    info!("Parsed table version {} with {} entries", ct.version, ct.entries.len());
                    let script = ct.lua_script.clone();
                    (
                        ct.entries.iter().map(MemoryRecord::from_cheat_entry).collect(),
                        f.filename.clone(),
                        script,
                    )
                }
                Err(e) => {
                    warn!("Failed to parse embedded table: {}", e);
                    (Vec::new(), f.filename.clone(), None)
                }
            },
            None => {
                warn!("Overlay present but contained no HTT_TRAINER.CETRAINER entry");
                (Vec::new(), "".to_string(), None)
            }
        }
    } else {
        let table_path = if args.len() > 1 && !args[1].starts_with("--") {
            PathBuf::from(&args[1])
        } else {
            // Default when launched by the unpacker with no argument.
            let new = PathBuf::from("HTT_TRAINER.CETRAINER");
            if new.exists() { new } else { PathBuf::from("CET_TRAINER.CETRAINER") }
        };

        if table_path.exists() {
            info!("Loading table: {:?}", table_path);
            let name = table_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            match ct_parser::parse_file(table_path.to_str().unwrap_or("")) {
                Ok(ct) => {
                    info!("Parsed table version {} with {} entries", ct.version, ct.entries.len());
                    let script = ct.lua_script.clone();
                    (
                        ct.entries.iter().map(MemoryRecord::from_cheat_entry).collect(),
                        name,
                        script,
                    )
                }
                Err(e) => {
                    warn!("Failed to parse table {:?}: {}", table_path, e);
                    (Vec::new(), name, None)
                }
            }
        } else {
            warn!("Table file {:?} not found", table_path);
            (Vec::new(), "".to_string(), None)
        }
    };

    // On by default, matching the established behaviour; see
    // `RuntimeState::auto_activate`. Logged either way, because "nothing
    // happened when it attached" has to be explainable from the log alone.
    let auto_activate = env::var("HTT_AUTO_ACTIVATE")
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true);
    if auto_activate {
        info!("the table may enable its own records at attach, as expected (HTT_AUTO_ACTIVATE=0 to hold them back)");
    } else {
        info!("HTT_AUTO_ACTIVATE=0: the table's own enable requests are held back; features activate on request only");
    }

    let state: SharedState = Arc::new(Mutex::new(RuntimeState {
        process: None,
        symbols: symbol_handler::SymbolHandler::new(),
        records,
        auto_attach_target: None,
        table_name: table_filename,
        trainer_exe_name: self_exe_name,
        lua: lua_engine::LuaEngine::new(),
        pending_activations: std::collections::VecDeque::new(),
        auto_activate,
    }));

    // A table's <LuaScript> defines the helper functions its Auto Assembler
    // scripts call. It always runs for .CETRAINER files
    // ("cetrainers always execute the script"), so features break
    // in confusing ways if it is skipped.
    run_table_script(&state, table_script);

    // Un-patch the game on a normal shutdown. Leaving hooks behind means the
    // next run's AOB scans find their own jmp instead of the pattern they are
    // looking for, which looks exactly like a broken table.
    spawn_shutdown_cleanup(Arc::clone(&state));

    // Start background tasks
    freeze_loop::start_freeze_loop(Arc::clone(&state));
    hotkeys::start_hotkey_listener(Arc::clone(&state));

    // `--port N` allows a second trainer alongside one already bound to 42069.
    let port = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(42069);

    if let Err(e) = start_rest_server(port, state).await {
        // Bare "os error 10048" is unhelpful; name the actual problem.
        if e.to_string().contains("10048") || e.to_string().contains("in use") {
            anyhow::bail!(
                "port {} is already in use - another trainer is running.                  Stop it, or pass --port <n> to use a different one.",
                port
            );
        }
        anyhow::bail!("{}", e);
    }

    Ok(())
}

/// Writes an embedded `defines.lua` next to the running executable, once.
///
/// Nothing in this engine reads `defines.lua` itself - Tables that use it
/// do so via their own `dofile("defines.lua")` relative to the trainer's
/// working directory, a convention from the two-stage Windows unpacker that
/// always ran from the directory it had just extracted. A single overlay
/// binary has no such directory, so this recreates just that one assumption
/// without unpacking everything else. Never overwrites an existing file: a
/// user-edited `defines.lua` from a previous run must not be clobbered.
fn write_sibling_defines_lua(files: &[overlay::OverlayFile]) {
    let Some(defines) = files.iter().find(|f| f.filename == "defines.lua") else {
        return;
    };
    let Ok(exe) = env::current_exe() else { return };
    let Some(dir) = exe.parent() else { return };
    let target = dir.join("defines.lua");
    if target.exists() {
        return;
    }
    match std::fs::write(&target, &defines.data) {
        Ok(()) => info!("Wrote {:?} alongside the trainer", target),
        Err(e) => warn!("Could not write {:?}: {}", target, e),
    }
}

/// Reports Auto Assembler coverage for a table: how many scripts assemble
/// cleanly, and what the unsupported ones trip over.
fn analyze_table(path: &str, filter: Option<&str>) -> Result<()> {
    // A directory aggregates over every .CT inside it.
    let p = PathBuf::from(path);
    if p.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&p)?
            .flatten()
            .map(|e| e.path())
            .filter(|f| {
                f.extension()
                    .map(|x| x.eq_ignore_ascii_case("ct"))
                    .unwrap_or(false)
            })
            .collect();
        files.sort();

        let (mut tot, mut asm, mut lua, mut bad, mut errs) = (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
        let mut parse_errors: std::collections::BTreeMap<String, usize> = Default::default();

        for f in &files {
            match analyze_one(f.to_str().unwrap_or(""), None) {
                Ok(s) => {
                    tot += s.total;
                    asm += s.assembled;
                    lua += s.lua;
                    bad += s.failed;
                    for (k, v) in s.reasons {
                        *reasons.entry(k).or_insert(0) += v;
                    }
                }
                Err(e) => {
                    errs += 1;
                    let msg = format!("{:#}", e);
                    let key = msg.chars().take(90).collect::<String>();
                    *parse_errors.entry(key).or_insert(0) += 1;
                }
            }
        }

        println!("Tables scanned      : {} ({} unparseable)", files.len(), errs);
        println!("Auto Assembler scripts: {}", tot);
        println!("  assemble cleanly  : {}", asm);
        println!("  Lua-bodied        : {} (executed by the Lua engine)", lua);
        println!("  unsupported       : {}", bad);
        if tot > lua {
            let denom = (tot - lua) as f64;
            println!("  coverage of non-Lua scripts: {:.1}%", asm as f64 / denom * 100.0);
            let handled = asm + lua;
            println!("  handled overall            : {:.1}%", handled as f64 / tot as f64 * 100.0);
        }
        if !parse_errors.is_empty() {
            println!("\nParse failures:");
            let mut v: Vec<_> = parse_errors.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            for (reason, count) in v.iter().take(15) {
                println!("  {:>5}x {}", count, reason);
            }
        }
        if !reasons.is_empty() {
            println!("\nTop blockers:");
            let mut v: Vec<_> = reasons.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            for (reason, count) in v.iter().take(20) {
                println!("  {:>5}x {}", count, reason);
            }
        }
        return Ok(());
    }

    let s = analyze_one(path, filter)?;
    println!("Table: {}", path);
    println!("Auto Assembler scripts: {}", s.total);
    println!("  assemble cleanly : {}", s.assembled);
    println!("  Lua-bodied       : {} (executed by the Lua engine)", s.lua);
    println!("  unsupported      : {}", s.failed);
    if !s.reasons.is_empty() {
        println!("\nTop blockers:");
        let mut v: Vec<_> = s.reasons.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (reason, count) in v.iter().take(15) {
            println!("  {:>4}x {}", count, reason);
        }
    }
    Ok(())
}

struct AnalyzeStats {
    total: usize,
    lua: usize,
    assembled: usize,
    failed: usize,
    reasons: std::collections::BTreeMap<String, usize>,
}

fn analyze_one(path: &str, filter: Option<&str>) -> Result<AnalyzeStats> {
    use ct_types::{CheatEntry, VariableType};
    use std::collections::BTreeMap;

    let table = ct_parser::parse_file(path)?;

    let mut st = AnalyzeStats {
        total: 0,
        lua: 0,
        assembled: 0,
        failed: 0,
        reasons: BTreeMap::new(),
    };

    fn walk(entries: &[CheatEntry], st: &mut dyn FnMut(&CheatEntry)) {
        for e in entries {
            st(e);
            walk(&e.children, st);
        }
    }

    let mut visit = |e: &CheatEntry| {
        if e.var_type != VariableType::AutoAssembler {
            return;
        }
        let Some(script) = e.assembler_script.as_ref() else { return };
        st.total += 1;

        let matches_filter = filter
            .map(|f| e.description.to_lowercase().contains(&f.to_lowercase()))
            .unwrap_or(false);

        let parsed = auto_assembler::AutoAssemblerScript::parse(script);
        if parsed.is_lua {
            st.lua += 1;
            if matches_filter {
                println!("[LUA ] {}", e.description);
            }
            return;
        }

        let all_lines = auto_assembler::preprocess_for_analysis(&parsed.enable_lines);

        // Data directives are handled outside the encoder, but their values
        // still have to parse. Skipping them here is how `dd (float)1` hid: the
        // analyzer reported a clean table while the real engine aborted on it.
        let mut data_error: Option<String> = None;
        for line in &all_lines {
            let lower = line.trim().to_lowercase();
            let is_data = ["db ", "dw ", "dd ", "dq "]
                .iter()
                .any(|p| lower.starts_with(p));
            if !is_data {
                continue;
            }
            let rest = &line.trim()[3..];
            // Same tokenizer the emit path uses. These used to be two separate
            // copies that both split on space, and a divergence between them
            // means the analyser cheerfully reports a table the engine cannot
            // actually assemble - which is the exact failure the comment above
            // describes.
            let width = if lower.starts_with("db ") {
                1
            } else if lower.starts_with("dw ") {
                2
            } else {
                4
            };
            for tok in auto_assembler::split_data_operands(rest) {
                let t = tok.trim();
                // A quoted string is characters. One is allowed in db and dw
                // only; dd/dq have no
                // string branch at all.
                // `db ?` / `+N` are valid; they just need a live target, so the
                // offline analyser cannot evaluate them and must not report them
                // as unsupported either.
                if width == 1 && auto_assembler::is_byte_patch_operand(t) {
                    continue;
                }
                if auto_assembler::quoted_literal(t).is_some() {
                    if width > 2 {
                        data_error =
                            Some(format!("string literal not allowed in this directive: {}", t));
                    }
                    continue;
                }
                if assembler::parse_number(t).is_none() && t.parse::<i64>().is_err() {
                    // A bare identifier is a symbol, resolved at inject time -
                    // and so is a `symbol+offset` expression, which the emit
                    // path resolves through `resolve_expr`.
                    let is_symbol_char =
                        |c: char| c.is_alphanumeric() || c == '_' || c == '+' || c == '-';
                    let symbolish = t.chars().all(is_symbol_char)
                        && t.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_');
                    if !symbolish {
                        data_error = Some(format!("invalid data value '{}'", t));
                    }
                }
            }
            if data_error.is_some() {
                break;
            }
        }

        let lines: Vec<String> = all_lines
            .into_iter()
            .filter(|l| !auto_assembler::is_non_code_line(l))
            .collect();

        // Try a 32-bit-style image base and a 64-bit one. Legacy tables branch
        // to literal low addresses (`jmp 004956FE`), which are unencodable from
        // a high base purely because of where this harness pretends to be -
        // that is an artifact of the analysis, not a real gap.
        const BASES: [u64; 2] = [0x0040_0000, 0x7FF6_0100_0000];
        let mut last_err: Option<(String, String)> = last_err_from_data(&data_error);
        let mut best = 0usize;
        let mut ok = data_error.is_none();
        if data_error.is_some() {
            st.failed += 1;
            if let Some(msg) = &data_error {
                *st.reasons.entry(msg.clone()).or_insert(0) += 1;
                if matches_filter {
                    println!("[FAIL] {}
         {}", e.description, msg);
                }
            }
            return;
        }

        for base in BASES {
            let resolve = |_: &str| Some(base);
            let mut count = 0usize;
            let mut failed = None;
            let mut addr = base;

            for line in &lines {
                match assembler::assemble(line, addr, &resolve) {
                    Ok(bytes) => {
                        count += 1;
                        addr += bytes.len().max(1) as u64;
                    }
                    Err(err) => {
                        failed = Some((format!("{:#}", err), line.clone()));
                        break;
                    }
                }
            }

            match failed {
                None => {
                    ok = true;
                    best = count;
                    break;
                }
                Some(f) => {
                    best = best.max(count);
                    last_err = Some(f);
                }
            }
        }

        if ok {
            st.assembled += 1;
            if matches_filter {
                println!("[ OK ] {} ({} instructions)", e.description, best);
            }
        } else {
            st.failed += 1;
            if let Some((msg, _line)) = last_err {
                // Key on the mnemonic for unsupported instructions so the
                // report shows what to implement next.
                let key = if let Some(i) = msg.find("unsupported instruction: '") {
                    let rest = &msg[i + 26..];
                    let mnem = rest.split([' ', '\'']).next().unwrap_or("?").to_lowercase();
                    format!("unsupported instruction: {}", mnem)
                } else {
                    msg.split_once(':').map(|(a, _)| a.to_string()).unwrap_or(msg.clone())
                };
                *st.reasons.entry(key).or_insert(0) += 1;
                if matches_filter {
                    println!("[FAIL] {}\n         {}", e.description, msg);
                }
            }
        }
    };

    walk(&table.entries, &mut visit);
    Ok(st)
}

/// Executes every Lua block in a table and classifies the outcome.
///
/// `--analyze` only *counts* Lua scripts; it never runs them, so it cannot say
/// whether they work. This does run them, with no process attached, and sorts
/// the results into:
///
/// - **ok**            - ran to completion
/// - **needs game**    - failed only because nothing is attached; these are the
///                       ones that should work once the game is running
/// - **missing API**   - called a table Lua function this runtime does not provide
/// - **syntax error**  - does not even compile
/// - **other**         - genuine runtime errors
fn check_lua(path: &str) -> Result<()> {
    use ct_types::{CheatEntry, VariableType};
    use lua_engine::{LuaCtx, LuaEngine};
    use std::collections::BTreeMap;

    let table = ct_parser::parse_file(path)?;

    let mut flat: Vec<CheatEntry> = Vec::new();
    fn walk(entries: &[CheatEntry], out: &mut Vec<CheatEntry>) {
        for e in entries {
            out.push(e.clone());
            walk(&e.children, out);
        }
    }
    walk(&table.entries, &mut flat);

    let views: Vec<lua_engine::RecordView> = flat
        .iter()
        .map(|e| lua_engine::RecordView {
            id: e.id,
            description: e.description.clone(),
            address_expr: e.address.clone().unwrap_or_default(),
            active: false,
            is_aa: e.var_type == VariableType::AutoAssembler,
            child_ids: e.children.iter().map(|c| c.id).collect(),
        })
        .collect();

    let shared_engine = LuaEngine::new();
    shared_engine.set_instruction_limit(200);

    // The table's own <LuaScript> defines the helpers its scripts call, and the
    // runtime runs it at startup. Skipping it here would report table-defined
    // functions as missing engine APIs.
    if let Some(script) = table.lua_script.clone() {
        let chunks: Vec<String> = match script {
            ct_types::LuaScript::Single(s) => vec![s],
            ct_types::LuaScript::MultiTab(tabs) => tabs.into_iter().map(|(_, s)| s).collect(),
        };
        let _ = shared_engine.set_global_string("TrainerOrigin", path);
        for chunk in chunks {
            let mut syms = symbol_handler::SymbolHandler::new();
            let mut ctx = LuaCtx {
                process: None,
                symbols: &mut syms,
                records: views.clone(),
                actions: Vec::new(),
            };
            if let Err(e) = shared_engine.run_block(&chunk, None, &mut ctx) {
                eprintln!("[TABLE SCRIPT] {}", e);
            }
        }
    }

    let (mut ok, mut needs_game, mut syntax, mut other) = (0usize, 0usize, 0usize, 0usize);
    let mut missing: BTreeMap<String, usize> = BTreeMap::new();
    let mut missing_scripts = 0usize;
    let mut other_msgs: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0usize;

    for e in &flat {
        if e.var_type != VariableType::AutoAssembler {
            continue;
        }
        let Some(script) = e.assembler_script.as_ref() else { continue };
        let parsed = auto_assembler::AutoAssemblerScript::parse(script);
        if !parsed.is_lua {
            continue;
        }

        let mut chunks = auto_assembler::extract_lua_blocks(&parsed.enable_lines);
        chunks.extend(auto_assembler::extract_lua_calls(&parsed.enable_lines));
        if chunks.is_empty() {
            continue;
        }
        total += 1;

        // One shared engine, matching the runtime: scripts routinely
        // define helpers that sibling scripts call, so isolating them reports
        // table-defined globals as if they were missing engine APIs.
        let engine = &shared_engine;

        let mut verdict_ok = true;
        let mut saw_missing = false;

        for code in &chunks {
            if engine.syntax_check(code).is_err() {
                syntax += 1;
                verdict_ok = false;
                break;
            }

            let mut syms = symbol_handler::SymbolHandler::new();
            let mut ctx = LuaCtx {
                process: None,
                symbols: &mut syms,
                records: views.clone(),
                actions: Vec::new(),
            };

            if let Err(err) = engine.run_block(code, None, &mut ctx) {
                let msg = err.to_string();
                if msg.contains("not attached to a process") {
                    needs_game += 1;
                    verdict_ok = false;
                    break;
                }
                // "attempt to call a nil value (global 'X')" => X is unimplemented.
                if let Some(i) = msg.find("(global '") {
                    let rest = &msg[i + 9..];
                    if let Some(end) = rest.find('\'') {
                        *missing.entry(rest[..end].to_string()).or_insert(0) += 1;
                        saw_missing = true;
                        verdict_ok = false;
                        break;
                    }
                }
                let key: String = msg
                    .lines()
                    .next()
                    .unwrap_or(&msg)
                    .chars()
                    .take(80)
                    .collect();
                *other_msgs.entry(key).or_insert(0) += 1;
                eprintln!("[OTHER] {} :: {}", e.description, msg.lines().next().unwrap_or(""));
                other += 1;
                verdict_ok = false;
                break;
            }
        }

        if saw_missing {
            missing_scripts += 1;
        }
        if verdict_ok {
            ok += 1;
        }
    }

    println!("Table: {}", path);
    println!("Lua-bearing scripts : {}", total);
    println!("  ran clean         : {}", ok);
    println!("  need a live game  : {}", needs_game);
    println!("  missing table API : {}", missing_scripts);
    println!("  syntax errors     : {}", syntax);
    println!("  other errors      : {}", other);

    if !missing.is_empty() {
        println!("\nUnimplemented table Lua functions:");
        let mut v: Vec<_> = missing.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (name, count) in v.iter().take(30) {
            println!("  {:>5}x {}", count, name);
        }
    }
    if !other_msgs.is_empty() {
        println!("\nOther runtime errors:");
        let mut v: Vec<_> = other_msgs.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (msg, count) in v.iter().take(12) {
            println!("  {:>5}x {}", count, msg);
        }
    }
    Ok(())
}

/// Runs the table-level `<LuaScript>` once at startup.
fn run_table_script(state: &SharedState, script: Option<ct_types::LuaScript>) {
    use ct_types::LuaScript;
    use lua_engine::LuaCtx;

    let Some(script) = script else { return };
    let chunks: Vec<String> = match script {
        LuaScript::Single(s) => vec![s],
        LuaScript::MultiTab(tabs) => tabs.into_iter().map(|(_, s)| s).collect(),
    };

    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let st = &mut *guard;

    // Table scripts commonly gate on this to detect a generated trainer.
    if let Err(e) = st.lua.set_global_string("TrainerOrigin", &st.table_name) {
        warn!("could not set TrainerOrigin: {}", e);
    }

    // The whole load is bracketed with these (the closing one in a
    // `finally`), and executes the table's own script in between
    // (*"cetrainers always execute the script"*). The
    // opening call therefore cannot reach a handler the table itself defines -
    // that is the behaviour too, not an omission here - but `tableIsLoading`
    // is visible to the script while it runs, and some tables branch on it.
    let _ = st.lua.set_global_bool("tableIsLoading", true);
    {
        let views = rest_server::collect_record_views(&st.records);
        let mut ctx = LuaCtx {
            process: st.process.as_mut(),
            symbols: &mut st.symbols,
            records: views,
            actions: Vec::new(),
        };
        st.lua.call_callback("onTableLoad", vec![mlua::Value::Boolean(true)], &mut ctx);
    }

    let views = rest_server::collect_record_views(&st.records);
    for chunk in chunks {
        if chunk.trim().is_empty() {
            continue;
        }
        let mut ctx = LuaCtx {
            process: st.process.as_mut(),
            symbols: &mut st.symbols,
            records: views.clone(),
            actions: Vec::new(),
        };
        match st.lua.run_block(&chunk, None, &mut ctx) {
            Ok(_) => info!("Table Lua script executed ({} bytes)", chunk.len()),
            Err(e) => warn!("Table Lua script failed: {}", e),
        }

        // The table may name the game via getAutoAttachList(); honour it so the
        // trainer connects on its own once the game starts.
        //
        // Everything else the script asked for has to be carried out too. This
        // drain used to match `SetAutoAttach` alone and silently discard the
        // rest, so a table that activated a record from its top-level script
        // was ignored - with nothing logged to say so.
        let mut queued = std::mem::take(&mut ctx.actions);
        if !st.auto_activate {
            rest_server::hold_back_table_enables(&mut queued, &st.records);
        }

        let mut deferred = Vec::new();
        for action in queued {
            match action {
                lua_engine::LuaAction::SetAutoAttach { process } => {
                    if st.auto_attach_target.is_none() {
                        info!("Auto-attach target from table: {}", process);
                        st.auto_attach_target = Some(process);
                    }
                }
                other => deferred.push(other),
            }
        }

        if !deferred.is_empty() {
            match st.process {
                Some(ref mut proc) => rest_server::apply_lua_actions(
                    deferred,
                    &mut st.records,
                    proc,
                    &mut st.symbols,
                    &st.lua,
                    &mut Vec::new(),
                ),
                // The table script runs before any attach, so a record action
                // here has no process to act on. The original is in the same position at
                // table-load time and simply fails the activation; say so
                // rather than dropping it without a trace. The table's real
                // initialisation hook is `onOpenProcess`, which does have one.
                None => warn!(
                    "table script requested {} record action(s) before any process was attached; \
                     ignoring (tables should do this from onOpenProcess)",
                    deferred.len()
                ),
            }
        }
    }

    // the closing call is in a `finally`, so it runs even when the load threw;
    // reaching it unconditionally here is the same guarantee.
    let _ = st.lua.set_global_bool("tableIsLoading", false);
    let views = rest_server::collect_record_views(&st.records);
    let mut ctx = LuaCtx {
        process: st.process.as_mut(),
        symbols: &mut st.symbols,
        records: views,
        actions: Vec::new(),
    };
    if st.lua.call_callback("onTableLoad", vec![mlua::Value::Boolean(false)], &mut ctx) {
        info!("table's onTableLoad ran");
    }
}

/// Threads a data-directive error into the same reporting path as encoder errors.
fn last_err_from_data(d: &Option<String>) -> Option<(String, String)> {
    d.as_ref().map(|m| (m.clone(), String::new()))
}

/// Restores original bytes for every active script when the process is asked to
/// stop (Ctrl+C, console close, or logoff/shutdown).
///
/// A hard `Stop-Process -Force` cannot be intercepted, so this is best-effort -
/// but it covers every ordinary way a trainer gets closed.
fn spawn_shutdown_cleanup(state: SharedState) {
    tokio::spawn(async move {
        #[cfg(windows)]
        {
            use tokio::signal::windows;
            let mut ctrl_c = match windows::ctrl_c() {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut close = match windows::ctrl_close() {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut shutdown = match windows::ctrl_shutdown() {
                Ok(s) => s,
                Err(_) => return,
            };
            tokio::select! {
                _ = ctrl_c.recv() => {}
                _ = close.recv() => {}
                _ = shutdown.recv() => {}
            }
        }
        #[cfg(not(windows))]
        {
            // SIGTERM is how every service manager and plain `kill` asks a
            // process to stop, and SIGHUP fires on session teardown (closing
            // the terminal that launched the trainer). Neither is covered by
            // ctrl_c() alone, and a trainer killed either way leaves its hooks
            // in the game - the next run's AOB scan then finds its own jmp
            // instead of the pattern it wants. SIGKILL stays uninterceptable,
            // same as on Windows.
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut hup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(_) => return,
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
        }

        info!("Shutting down: restoring original bytes...");
        let restored = deactivate_everything(&state);
        info!("Restored {} script(s)", restored);
        std::process::exit(0);
    });
}

/// Turns every active script off, deepest child first.
fn deactivate_everything(state: &SharedState) -> usize {
    use lua_engine::ExecEnv;
    use memory_record::MemoryRecord;

    fn walk(
        rec: &mut MemoryRecord,
        process: &mut process::GameProcess,
        env: &mut ExecEnv<'_>,
        n: &mut usize,
    ) {
        for child in &mut rec.children {
            walk(child, process, env, n);
        }
        if rec.active && rec.set_active(false, process, env).is_ok() {
            *n += 1;
        }
    }

    let Ok(mut guard) = state.lock() else { return 0 };
    let st = &mut *guard;
    let Some(proc) = st.process.as_mut() else { return 0 };

    let views = rest_server::collect_record_views(&st.records);
    let mut n = 0;
    let mut env = ExecEnv::new(&mut st.symbols, &st.lua, &views);
    for rec in &mut st.records {
        walk(rec, proc, &mut env, &mut n);
    }
    n
}

/// Differential test of the assembler against the disassembly tables embed.
///
/// Tables embed the original code around each injection point as a comment:
///
/// ```text
/// "RDR2.exe"+6E66BE: E8 65 BC 21 02  -  call RDR2.exe+2902328
/// ```
///
/// Those are byte/mnemonic pairs produced by the original tooling, so assembling the text and
/// comparing against the bytes checks this encoder against a real one - at
/// scale, with no game running. Both the instruction address and any
/// `module+offset` operand resolve from the same synthetic base, so relative
/// displacements come out identical to the.
fn verify_asm(path: &str) -> Result<()> {
    use ct_types::CheatEntry;
    use std::collections::BTreeMap;

    const BASE: u64 = 0x1_4000_0000;

    let mut files: Vec<PathBuf> = Vec::new();
    let p = PathBuf::from(path);
    if p.is_dir() {
        files = std::fs::read_dir(&p)?
            .flatten()
            .map(|e| e.path())
            .filter(|f| f.extension().map(|x| x.eq_ignore_ascii_case("ct")).unwrap_or(false))
            .collect();
        files.sort();
    } else {
        files.push(p);
    }

    let (mut total, mut matched, mut mismatched, mut unsupported) = (0usize, 0usize, 0usize, 0usize);
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut samples: Vec<String> = Vec::new();

    for file in &files {
        let Ok(table) = ct_parser::parse_file(file.to_str().unwrap_or("")) else { continue };

        let mut scripts: Vec<String> = Vec::new();
        fn collect(e: &[CheatEntry], out: &mut Vec<String>) {
            for x in e {
                if let Some(s) = &x.assembler_script {
                    out.push(s.clone());
                }
                collect(&x.children, out);
            }
        }
        collect(&table.entries, &mut scripts);

        for script in scripts {
            for line in script.lines() {
                let Some((addr_part, rest)) = line.split_once(':') else { continue };
                let Some((bytes_part, insn)) = rest.split_once('-') else { continue };

                // Address must look like `module+hexoffset`.
                let a = addr_part.trim().replace('"', "");
                let Some((module, off)) = a.rsplit_once('+') else { continue };
                if module.is_empty() || !module.contains('.') {
                    continue;
                }
                let Ok(offset) = u64::from_str_radix(off.trim(), 16) else { continue };

                // Byte column: two-hex-digit tokens only.
                let toks: Vec<&str> = bytes_part.split_whitespace().collect();
                if toks.len() < 2 || toks.len() > 15 {
                    continue;
                }
                let mut expected = Vec::new();
                let mut good = true;
                for t in &toks {
                    match u8::from_str_radix(t, 16) {
                        Ok(b) if t.len() == 2 => expected.push(b),
                        _ => { good = false; break; }
                    }
                }
                if !good || expected.is_empty() {
                    continue;
                }

                let insn = insn.trim();
                if insn.is_empty() || insn.starts_with("//") {
                    continue;
                }

                let addr = BASE + offset;
                let module_owned = module.to_string();
                // Mirror the real symbol handler: a bare module name, or
                // `module+hexoffset`. Splitting on '-' would break the many
                // modules named like `Palworld-Win64-Shipping.exe`.
                let resolve = move |name: &str| -> Option<u64> {
                    let n = name.trim().replace('"', "");
                    if n.eq_ignore_ascii_case(&module_owned) {
                        return Some(BASE);
                    }
                    if let Some((m, off)) = n.rsplit_once('+') {
                        if m.trim().eq_ignore_ascii_case(&module_owned) {
                            if let Ok(o) = u64::from_str_radix(off.trim(), 16) {
                                return Some(BASE + o);
                            }
                        }
                    }
                    None
                };

                total += 1;
                match assembler::assemble(insn, addr, &resolve) {
                    Ok(got) => {
                        if got == expected {
                            matched += 1;
                        } else {
                            mismatched += 1;
                            if samples.len() < 25 {
                                samples.push(format!(
                                    "  {:<34} want={:<26} ours={}",
                                    insn,
                                    expected.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" "),
                                    got.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" "),
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        unsupported += 1;
                        let msg = format!("{:#}", e);
                        let key = if let Some(i) = msg.find("unsupported instruction: '") {
                            let rest = &msg[i + 26..];
                            format!("unsupported: {}", rest.split([' ', '\'']).next().unwrap_or("?").to_lowercase())
                        } else {
                            msg.split(':').next().unwrap_or(&msg).to_string()
                        };
                        *reasons.entry(key).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    println!("Instructions compared against the embedded disassembly: {}", total);
    if total > 0 {
        println!("  byte-identical : {}  ({:.2}%)", matched, matched as f64 / total as f64 * 100.0);
        println!("  different bytes: {}  ({:.2}%)", mismatched, mismatched as f64 / total as f64 * 100.0);
        println!("  not assembled  : {}  ({:.2}%)", unsupported, unsupported as f64 / total as f64 * 100.0);
    }
    if !samples.is_empty() {
        println!("\nEncoding differences (first {}):", samples.len());
        for s in &samples { println!("{}", s); }
    }
    if !reasons.is_empty() {
        println!("\nNot assembled, by cause:");
        let mut v: Vec<_> = reasons.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, n) in v.iter().take(18) { println!("  {:>6}x {}", n, k); }
    }
    Ok(())
}
