use crate::memory_record::{MemoryRecord, VariableType};
use crate::process::GameProcess;
use crate::lua_engine::{ExecEnv, LuaAction, LuaCtx, LuaEngine, RecordView};
use crate::symbol_handler::SymbolHandler;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

pub struct RuntimeState {
    pub process: Option<GameProcess>,
    pub symbols: SymbolHandler,
    pub records: Vec<MemoryRecord>,
    pub auto_attach_target: Option<String>,
    pub table_name: String,
    pub trainer_exe_name: String,
    /// Persistent across scripts, so globals a table defines stay visible to
    /// later scripts - the same as the single Lua state.
    pub lua: LuaEngine,
    /// Children queued by a `moActivateChildrenAsWell` parent, activated one
    /// per freeze-loop tick. See `ExecEnv::deferred_activations`.
    pub pending_activations: std::collections::VecDeque<(u32, bool)>,
    /// Whether the *table* is allowed to enable its own records.
    ///
    /// **On by default, matching established behaviour.** `TProcessHandler.Open` calls
    /// `onOpenProcess`, and RDR2's table uses
    /// that to arm a 1 s timer which sets its root group active; the group's
    /// `moActivateChildrenAsWell` then takes nine scripts up with it.
    ///
    /// This was off for a while, on the reasoning that a gamepad-driven trainer
    /// should inject only when its user says so. That turned out to be a
    /// mistake, and an instructive one: **tables are written against the
    /// activation that actually happens.** RDR2's teleport reads `coord2`, a
    /// pointer captured by a hook in `BASECoords`, which the table only ever
    /// expects to be installed as part of that startup cascade - and its
    /// sibling `Auto Teleport BASE` is in the same cascade. Holding the cascade
    /// back left the feature dereferencing a null pointer and killing the game,
    /// with nothing in the engine at fault. Suppressing a table's own
    /// initiative does not give you the same table with less injection; it
    /// gives you a table running in a state its author never had.
    ///
    /// `HTT_AUTO_ACTIVATE=0` restores the hold-back, which is still the right
    /// tool for bisecting a table that kills a game - one script at a time,
    /// from the API.
    ///
    /// This gates *initiative*, not mechanism: a record the user toggles always
    /// cascades to its children, in both modes, as expected.
    pub auto_activate: bool,
}

/// Removes the enable requests the table made on its own initiative, returning
/// how many were held back.
///
/// Applied at the three places table code runs unprompted - the table script at
/// load, `onOpenProcess` at attach, and timers/coroutines on the tick - and at
/// none of the places the user asks for something. `LuaAction::SetValue` is
/// left alone (writing a value is not injecting a cheat), and so is
/// `SetActive { active: false }`: a table's panic key or cleanup path turning
/// something *off* is never the thing a player needs protecting from.
pub fn hold_back_table_enables(actions: &mut Vec<LuaAction>, records: &[MemoryRecord]) -> usize {
    let mut held = 0;

    actions.retain(|action| match action {
        LuaAction::SetActive { id, active: true } => {
            let desc = find_record(records, *id)
                .map(|r| r.description.as_str())
                .unwrap_or("<unknown record>");
            info!("not enabling '{}' (id {}): the table asked, not the user", desc, id);
            held += 1;
            false
        }
        _ => true,
    });

    held
}

fn find_record(records: &[MemoryRecord], id: u32) -> Option<&MemoryRecord> {
    for rec in records {
        if rec.id == id {
            return Some(rec);
        }
        if let Some(found) = find_record(&rec.children, id) {
            return Some(found);
        }
    }
    None
}

/// Flattens the record tree into the view Lua sees.
pub fn collect_record_views(records: &[MemoryRecord]) -> Vec<RecordView> {
    fn walk(recs: &[MemoryRecord], out: &mut Vec<RecordView>) {
        for r in recs {
            out.push(r.to_view());
            walk(&r.children, out);
        }
    }
    let mut out = Vec::new();
    walk(records, &mut out);
    out
}

/// Applies record changes a Lua script asked for.
///
/// Runs after the script finishes rather than during it, so toggling one record
/// from another record's script cannot re-enter the Auto Assembler mid-run.
/// Depth is bounded because an applied action can itself queue more.
/// Runs the table's `onOpenProcess` callback, as happens on every
/// successful attach.
///
/// `TProcessHandler.SetProcessHandle` calls `open` whenever the handle
/// becomes non-zero, and `TProcessHandler.Open` does two things:
/// publishes the target's module name as the
/// global `process`, then fires `onOpenProcess(pid)`.
///
/// This is not a nicety. It is where a table gets to initialise itself against
/// a live process, and tables lean on it hard - RDR2's arms a one-second timer
/// that activates its root group, whose `moActivateChildrenAsWell` then brings
/// up every "BASE" script the rest of the table depends on. Without this call
/// those scripts never run and the table looks broken in ways that point
/// everywhere except here.
///
/// Must be called with `state.process` already set, from every attach site.
pub fn on_process_opened(state: &mut RuntimeState) {
    let Some((pid, name)) = state.process.as_ref().map(|p| (p.pid, p.name.clone())) else {
        return;
    };

    // lua_setglobal(luavm, 'process') with the first module's file name.
    if let Err(e) = state.lua.set_global_string("process", &name) {
        warn!("could not publish 'process' global: {}", e);
    }

    let views = collect_record_views(&state.records);
    let mut pending = Vec::new();
    {
        let mut ctx = LuaCtx {
            process: state.process.as_mut(),
            symbols: &mut state.symbols,
            records: views,
            actions: Vec::new(),
        };
        let args = vec![mlua::Value::Integer(pid as i64)];
        if state.lua.call_callback("onOpenProcess", args, &mut ctx) {
            info!("table's onOpenProcess ran for pid {}", pid);
        }
        pending.append(&mut ctx.actions);
    }

    if !state.auto_activate {
        hold_back_table_enables(&mut pending, &state.records);
    }

    // The callback itself usually only *arms* a timer, so this drain is often
    // empty; the work lands later, on a freeze-loop tick.
    if !pending.is_empty() {
        let mut deferred = Vec::new();
        if let Some(ref mut proc) = state.process {
            apply_lua_actions(
                pending,
                &mut state.records,
                proc,
                &mut state.symbols,
                &state.lua,
                &mut deferred,
            );
        }
        state.pending_activations.extend(deferred);
    }
}

pub fn apply_lua_actions(
    mut actions: Vec<LuaAction>,
    records: &mut Vec<MemoryRecord>,
    process: &mut GameProcess,
    symbols: &mut SymbolHandler,
    lua: &LuaEngine,
    deferred: &mut Vec<(u32, bool)>,
) {
    for depth in 0..4 {
        if actions.is_empty() {
            return;
        }
        let views = collect_record_views(records);
        let mut next: Vec<LuaAction> = Vec::new();

        for action in actions.drain(..) {
            let mut env = ExecEnv::new(symbols, lua, &views);
            match action {
                LuaAction::SetActive { id, active } => {
                    if let Some(rec) = find_record_mut(records, Some(id), None) {
                        if let Err(e) = rec.set_active(active, process, &mut env) {
                            warn!("lua-requested toggle of record {} failed: {}", id, e);
                        }
                    }
                }
                LuaAction::SetAutoAttach { .. } => {}
                LuaAction::SetValue { id, value } => {
                    if let Some(rec) = find_record_mut(records, Some(id), None) {
                        if let Err(e) = rec.write_value(process, env.symbols, &value) {
                            warn!("lua-requested set of record {} failed: {}", id, e);
                        }
                    }
                }
            }
            next.append(&mut env.actions);
            deferred.append(&mut env.deferred_activations);
        }

        if next.is_empty() {
            return;
        }
        if depth == 3 {
            warn!("dropping {} lua record action(s): nesting too deep", next.len());
            return;
        }
        actions = next;
    }
}

pub type SharedState = Arc<Mutex<RuntimeState>>;

pub async fn start_rest_server(port: u16, state: SharedState) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).await?;
    info!("HATEOAS REST server listening on http://{}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state_clone = Arc::clone(&state);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(
                    io,
                    service_fn(move |req| {
                        let state = Arc::clone(&state_clone);
                        async move { handle_request(req, state).await }
                    }),
                )
                .await
            {
                error!("Connection error: {}", err);
            }
        });
    }
}

fn find_record_mut<'a>(records: &'a mut [MemoryRecord], id: Option<u32>, desc: Option<&str>) -> Option<&'a mut MemoryRecord> {
    for rec in records {
        if let Some(target_id) = id {
            if rec.id == target_id {
                return Some(rec);
            }
        } else if let Some(target_desc) = desc {
            if rec.description.eq_ignore_ascii_case(target_desc) {
                return Some(rec);
            }
        }
        if let Some(found) = find_record_mut(&mut rec.children, id, desc) {
            return Some(found);
        }
    }
    None
}

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    state: SharedState,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let method = req.method();
    let path = req.uri().path();

    // CORS preflight
    if method == Method::OPTIONS {
        return Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
            .header("Access-Control-Allow-Headers", "Content-Type")
            .body(Full::new(Bytes::from("")))
            .unwrap());
    }

    match (method, path) {
        (&Method::GET, "/api/features") => {
            // Never block the poll on the state lock.
            //
            // Activating an Auto Assembler script AOB-scans the game's whole
            // address space - 82 seconds, measured, for one RDR2 script - and
            // holds this lock throughout. Waiting here makes every poll time
            // out, so a trainer that is working perfectly reads as offline and
            // the panel offers to kill it. Killing it mid-injection is what
            // actually costs the player their game.
            //
            // So: answer immediately either way, and say which it is. `busy`
            // means "alive, still working" - the one thing a timeout cannot
            // distinguish from "dead".
            match state.try_lock() {
                Ok(guard) => Ok(json_response(StatusCode::OK, &build_features_json(&guard).to_string())),
                Err(std::sync::TryLockError::WouldBlock) => Ok(json_response(
                    StatusCode::OK,
                    &json!({ "busy": true, "features": [] }).to_string(),
                )),
                Err(std::sync::TryLockError::Poisoned(p)) => Ok(json_response(
                    StatusCode::OK,
                    &build_features_json(&p.into_inner()).to_string(),
                )),
            }
        }
        (&Method::POST, "/api/features/toggle") => {
            let body_bytes = match read_body(req).await {
                Ok(b) => b,
                Err(_) => return Ok(json_response(StatusCode::BAD_REQUEST, r#"{"error":"Invalid body"}"#)),
            };

            #[derive(Deserialize)]
            struct ToggleReq {
                id: Option<u32>,
                description: Option<String>,
                active: Option<bool>,
            }

            if let Ok(payload) = serde_json::from_slice::<ToggleReq>(&body_bytes) {
                let mut guard = state.lock().unwrap();
                let state_ref = &mut *guard;

                // Snapshot before borrowing a record mutably; Lua needs the
                // whole table and cannot hold a second borrow of it.
                let views = collect_record_views(&state_ref.records);
                let mut pending: Vec<LuaAction> = Vec::new();

                let mut cascade: Vec<(u32, bool)> = Vec::new();
                let record = find_record_mut(&mut state_ref.records, payload.id, payload.description.as_deref());

                if let Some(rec) = record {
                    let target_active = payload.active.unwrap_or(!rec.active);
                    let err_msg = if let Some(ref mut proc) = state_ref.process {
                        let mut env =
                            ExecEnv::new(&mut state_ref.symbols, &state_ref.lua, &views);
                        let e = rec
                            .set_active(target_active, proc, &mut env)
                            .err()
                            // `{:#}` includes the whole context chain; plain
                            // Display shows only the outermost message, which
                            // hides the actual cause.
                            .map(|e| format!("{:#}", e));
                        pending = std::mem::take(&mut env.actions);
                        // A `moActivateChildrenAsWell` record hands its children
                        // to the caller to schedule rather than running them
                        // inline (`memory_record.rs`). This path used to drop
                        // them, so toggling such a record brought up the record
                        // and nothing under it - invisible while the startup
                        // cascade was doing the same work, and the only way
                        // anything comes up now that it is not.
                        cascade = std::mem::take(&mut env.deferred_activations);
                        e
                    } else if rec.var_type == VariableType::AutoAssembler {
                        // Enabling a script needs a target. Reporting success
                        // here would claim an injection that never happened.
                        Some("Not attached to process".to_string())
                    } else {
                        rec.active = target_active;
                        None
                    };

                    let mut res = json!({ "id": rec.id, "description": rec.description, "active": rec.active });
                    if let Some(msg) = err_msg {
                        res["error"] = json!(msg);
                    }

                    if !pending.is_empty() {
                        if let Some(ref mut proc) = state_ref.process {
                            apply_lua_actions(
                                pending,
                                &mut state_ref.records,
                                proc,
                                &mut state_ref.symbols,
                                &state_ref.lua,
                                &mut cascade,
                            );
                        }
                    }
                    // Drained by the freeze loop, one per tick, so the reply
                    // goes back now rather than after a subtree of scripts.
                    state_ref.pending_activations.extend(cascade);
                    Ok(json_response(StatusCode::OK, &res.to_string()))
                } else {
                    Ok(json_response(StatusCode::NOT_FOUND, r#"{"error":"Feature not found"}"#))
                }
            } else {
                Ok(json_response(StatusCode::BAD_REQUEST, r#"{"error":"Invalid JSON"}"#))
            }
        }
        (&Method::POST, "/api/features/set") => {
            let body_bytes = match read_body(req).await {
                Ok(b) => b,
                Err(_) => return Ok(json_response(StatusCode::BAD_REQUEST, r#"{"error":"Invalid body"}"#)),
            };

            #[derive(Deserialize)]
            struct SetReq {
                id: Option<u32>,
                description: Option<String>,
                value: String,
            }

            if let Ok(payload) = serde_json::from_slice::<SetReq>(&body_bytes) {
                let mut guard = state.lock().unwrap();
                let state_ref = &mut *guard;

                let record = find_record_mut(&mut state_ref.records, payload.id, payload.description.as_deref());

                if let Some(rec) = record {
                    if let Some(ref proc) = state_ref.process {
                        match rec.write_value(proc, &state_ref.symbols, &payload.value) {
                            Ok(()) => {
                                let res = json!({ "id": rec.id, "description": rec.description, "value": payload.value });
                                Ok(json_response(StatusCode::OK, &res.to_string()))
                            }
                            Err(e) => {
                                let res = json!({ "error": e.to_string(), "id": rec.id });
                                Ok(json_response(StatusCode::BAD_REQUEST, &res.to_string()))
                            }
                        }
                    } else {
                        Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"Not attached to process"}"#))
                    }
                } else {
                    Ok(json_response(StatusCode::NOT_FOUND, r#"{"error":"Feature not found"}"#))
                }
            } else {
                Ok(json_response(StatusCode::BAD_REQUEST, r#"{"error":"Invalid JSON"}"#))
            }
        }
        // Enables every Auto Assembler script in the table. Records addressed by
        // AA-registered symbols (RDR2's Coord X/Y/Z use `_pPos`) cannot resolve
        // until the script that registers the symbol has run.
        (&Method::POST, "/api/features/activate_all") => {
            let mut guard = state.lock().unwrap();
            let state_ref = &mut *guard;

            let Some(ref mut proc) = state_ref.process else {
                return Ok(json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"error":"Not attached to process"}"#,
                ));
            };

            // Scripts depend on symbols other scripts register (RDR2's teleport
            // locations need BASEWaypoint, which needs BASECoords, which needs
            // Main CodeBase). Rather than demand the caller know the order, keep
            // retrying while each pass makes progress.
            let mut ok = Vec::new();
            let mut failed = Vec::new();
            let mut pending: Vec<LuaAction> = Vec::new();

            for _pass in 0..6 {
                let views = collect_record_views(&state_ref.records);
                let before = ok.len();
                failed.clear();
                {
                    let mut env = ExecEnv::new(&mut state_ref.symbols, &state_ref.lua, &views);
                    for rec in &mut state_ref.records {
                        rec.activate_scripts_recursive(proc, &mut env, &mut ok, &mut failed);
                    }
                    pending.append(&mut env.actions);
                }
                if failed.is_empty() || ok.len() == before {
                    break;
                }
            }
            apply_lua_actions(
                pending,
                &mut state_ref.records,
                proc,
                &mut state_ref.symbols,
                &state_ref.lua,
            &mut Vec::new(),
            );

            let failures: Vec<serde_json::Value> = failed
                .iter()
                .map(|(d, e)| json!({ "description": d, "error": e }))
                .collect();

            let res = json!({
                "activated": ok.len(),
                "failed": failures.len(),
                "failures": failures,
            });
            Ok(json_response(StatusCode::OK, &res.to_string()))
        }
        // Diagnostics: resolve an address expression and dump bytes there.
        // `GET /api/debug/read?expr=_pPos&size=16`
        (&Method::GET, "/api/debug/read") => {
            let query = req.uri().query().unwrap_or("").to_string();
            let mut expr = String::new();
            let mut size = 16usize;
            for pair in query.split('&') {
                match pair.split_once('=') {
                    Some(("expr", v)) => expr = percent_decode(v),
                    Some(("size", v)) => size = v.parse().unwrap_or(16).min(512),
                    _ => {}
                }
            }

            let guard = state.lock().unwrap();
            let Some(ref proc) = guard.process else {
                return Ok(json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"error":"Not attached"}"#,
                ));
            };

            match crate::address_parser::evaluate_address(&expr, Some(proc), &guard.symbols) {
                Ok(addr) => {
                    let mut buf = vec![0u8; size];
                    let read_ok = proc.read_memory(addr, &mut buf).is_ok();
                    let hex = if read_ok {
                        buf.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ")
                    } else {
                        String::new()
                    };
                    let res = json!({
                        "expr": expr,
                        "address": format!("{:X}", addr),
                        "readable": read_ok,
                        "bytes": hex,
                    });
                    Ok(json_response(StatusCode::OK, &res.to_string()))
                }
                Err(e) => {
                    let res = json!({ "expr": expr, "error": e.to_string() });
                    Ok(json_response(StatusCode::BAD_REQUEST, &res.to_string()))
                }
            }
        }
        // Every symbol currently registered by Auto Assembler scripts.
        (&Method::GET, "/api/debug/symbols") => {
            let guard = state.lock().unwrap();
            let mut names: Vec<serde_json::Value> = guard
                .symbols
                .all_symbols()
                .into_iter()
                .map(|(n, a)| json!({ "name": n, "address": format!("{:X}", a) }))
                .collect();
            names.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            let res = json!({ "count": names.len(), "symbols": names });
            Ok(json_response(StatusCode::OK, &res.to_string()))
        }
        // Disables every active script, restoring the original bytes.
        //
        // Killing a trainer leaves its hooks in the game: the patched bytes and
        // the VirtualAllocEx blocks both live in the target process, so a later
        // AOB scan no longer matches its own pattern. Call this before exiting.
        (&Method::POST, "/api/features/deactivate_all") => {
            let mut guard = state.lock().unwrap();
            let state_ref = &mut *guard;

            let Some(ref mut proc) = state_ref.process else {
                return Ok(json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"error":"Not attached to process"}"#,
                ));
            };

            let views = collect_record_views(&state_ref.records);
            let mut off = Vec::new();
            let mut failed = Vec::new();
            {
                let mut env = ExecEnv::new(&mut state_ref.symbols, &state_ref.lua, &views);
                for rec in &mut state_ref.records {
                    deactivate_recursive(rec, proc, &mut env, &mut off, &mut failed);
                }
            }

            let res = json!({
                "deactivated": off.len(),
                "failed": failed.len(),
                "failures": failed,
            });
            Ok(json_response(StatusCode::OK, &res.to_string()))
        }

        // Un-patch the game, then exit - the same clean path the signal handler
        // runs, but asked for over the API and, crucially, *acknowledged*.
        //
        // This exists because signalling a trainer is unreliable here. The
        // trainer is a Windows process under Proton: its shutdown handler waits
        // on console control events (`spawn_shutdown_cleanup`), and a Linux
        // SIGTERM delivered to the wine process does not dependably become one.
        // A trainer killed that way dies with its hooks still in the game, and
        // the *next* run's `aobscanmodule` then finds its own `jmp` instead of
        // the pattern it wants - every script fails to enable and it presents
        // as a broken table. That has cost real debugging time more than once.
        //
        // The caller gets the restore counts before the process goes, so it can
        // tell "unhooked cleanly" from "had to be killed" instead of guessing.
        (&Method::POST, "/api/shutdown") => {
            let mut off = Vec::new();
            let mut failed: Vec<String> = Vec::new();
            let attached;

            {
                let mut guard = state.lock().unwrap();
                let state_ref = &mut *guard;
                attached = state_ref.process.is_some();

                // Not attached is a normal shutdown, not an error: there is
                // nothing patched, so there is nothing to restore. Returning
                // 503 here (as deactivate_all does) would push the caller onto
                // the kill path for no reason.
                if let Some(ref mut proc) = state_ref.process {
                    let views = collect_record_views(&state_ref.records);
                    let mut env = ExecEnv::new(&mut state_ref.symbols, &state_ref.lua, &views);
                    for rec in &mut state_ref.records {
                        deactivate_recursive(rec, proc, &mut env, &mut off, &mut failed);
                    }
                }
            }

            info!(
                "shutdown requested over the API: restored {} script(s), {} failed",
                off.len(),
                failed.len()
            );

            // Exit *after* the response is on the wire. Returning from this
            // handler is what lets hyper write it, so the exit has to happen on
            // another task with enough delay to cover that write - it is a
            // small JSON body over loopback. Exiting inline would drop the
            // connection and the caller would fall back to killing us, undoing
            // the point of the endpoint.
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                info!("exiting");
                std::process::exit(0);
            });

            let res = json!({
                "ok": failed.is_empty(),
                "attached": attached,
                "deactivated": off.len(),
                "failed": failed.len(),
                "failures": failed,
                "exiting": true,
            });
            Ok(json_response(StatusCode::OK, &res.to_string()))
        }
        // Diagnostics: write raw bytes at an address expression.
        // `POST /api/debug/write  {"expr":"RDR2.exe+9C2467","bytes":"0F 10 59 70"}`
        (&Method::POST, "/api/debug/write") => {
            let body_bytes = match read_body(req).await {
                Ok(b) => b,
                Err(_) => return Ok(json_response(StatusCode::BAD_REQUEST, r#"{"error":"bad body"}"#)),
            };
            #[derive(Deserialize)]
            struct W {
                expr: String,
                bytes: String,
            }
            let Ok(p) = serde_json::from_slice::<W>(&body_bytes) else {
                return Ok(json_response(StatusCode::BAD_REQUEST, r#"{"error":"bad json"}"#));
            };

            let guard = state.lock().unwrap();
            let Some(ref proc) = guard.process else {
                return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"not attached"}"#));
            };

            let data: Result<Vec<u8>, _> = p
                .bytes
                .split_whitespace()
                .map(|t| u8::from_str_radix(t, 16))
                .collect();
            let Ok(data) = data else {
                return Ok(json_response(StatusCode::BAD_REQUEST, r#"{"error":"bad hex"}"#));
            };

            match crate::address_parser::evaluate_address(&p.expr, Some(proc), &guard.symbols) {
                Ok(addr) => {
                    let res = match proc.write_memory(addr, &data) {
                        Ok(()) => json!({ "address": format!("{:X}", addr), "wrote": data.len() }),
                        Err(e) => json!({ "address": format!("{:X}", addr), "error": e.to_string() }),
                    };
                    Ok(json_response(StatusCode::OK, &res.to_string()))
                }
                Err(e) => {
                    let res = json!({ "error": e.to_string() });
                    Ok(json_response(StatusCode::BAD_REQUEST, &res.to_string()))
                }
            }
        }
        // `GET /api/debug/scan?module=RDR2.exe&pattern=48+8B+03+4D+03+F6`
        (&Method::GET, "/api/debug/scan") => {
            let query = req.uri().query().unwrap_or("").to_string();
            let (mut module, mut pattern) = (String::new(), String::new());
            for pair in query.split('&') {
                match pair.split_once('=') {
                    Some(("module", v)) => module = percent_decode(v),
                    Some(("pattern", v)) => pattern = percent_decode(v),
                    _ => {}
                }
            }

            let guard = state.lock().unwrap();
            let Some(ref proc) = guard.process else {
                return Ok(json_response(StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"not attached"}"#));
            };

            let pat = match crate::aob_scanner::parse_aob_pattern(&pattern) {
                Ok(p) => p,
                Err(e) => {
                    let res = json!({ "error": e.to_string() });
                    return Ok(json_response(StatusCode::BAD_REQUEST, &res.to_string()));
                }
            };
            let m = if module.is_empty() { None } else { Some(module.as_str()) };
            match crate::aob_scanner::aob_scan_all(proc, &pat, m, 32) {
                Ok(hits) => {
                    let addrs: Vec<String> = hits.iter().map(|a| format!("{:X}", a)).collect();
                    let res = json!({ "matches": addrs.len(), "addresses": addrs });
                    Ok(json_response(StatusCode::OK, &res.to_string()))
                }
                Err(e) => {
                    let res = json!({ "error": e.to_string() });
                    Ok(json_response(StatusCode::BAD_REQUEST, &res.to_string()))
                }
            }
        }
        (&Method::POST, "/api/process/attach") => {
            let body_bytes = match read_body(req).await {
                Ok(b) => b,
                Err(_) => return Ok(json_response(StatusCode::BAD_REQUEST, r#"{"error":"Invalid body"}"#)),
            };

            #[derive(Deserialize)]
            struct AttachReq {
                process: String,
            }

            if let Ok(payload) = serde_json::from_slice::<AttachReq>(&body_bytes) {
                let mut guard = state.lock().unwrap();
                guard.auto_attach_target = Some(payload.process.clone());

                match GameProcess::attach_by_name(&payload.process) {
                    Ok(proc) => {
                        info!("Attached to {} (pid {})", proc.name, proc.pid);
                        guard.process = Some(proc);
                        on_process_opened(&mut guard);
                        Ok(json_response(StatusCode::OK, r#"{"status":"attached"}"#))
                    }
                    Err(e) => {
                        // Report why. Silently answering "waiting" made an
                        // access-denied look like the game was not running.
                        let res = json!({ "status": "waiting", "error": e.to_string() });
                        Ok(json_response(StatusCode::OK, &res.to_string()))
                    }
                }
            } else {
                Ok(json_response(StatusCode::BAD_REQUEST, r#"{"error":"Invalid JSON"}"#))
            }
        }
        _ => Ok(json_response(StatusCode::NOT_FOUND, r#"{"error":"Endpoint not found"}"#)),
    }
}

fn record_to_json(rec: &MemoryRecord, state: &RuntimeState) -> serde_json::Value {
    // Why a value is unavailable matters: a record addressed by an Auto
    // Assembler symbol reads as "??" until the script that registers that
    // symbol has been enabled. Reporting the reason makes that diagnosable
    // instead of looking like a wrong value.
    let mut read_error: Option<String> = None;
    let val = if let Some(ref proc) = state.process {
        match rec.read_value(proc, &state.symbols) {
            Ok(v) => v,
            Err(e) => {
                read_error = Some(e.to_string());
                "??".to_string()
            }
        }
    } else {
        "??".to_string()
    };

    let type_str = match rec.var_type {
        VariableType::Byte => "vtByte",
        VariableType::Word => "vtWord",
        VariableType::Dword => "vtDword",
        VariableType::Qword => "vtQword",
        VariableType::Float => "vtSingle",
        VariableType::Double => "vtDouble",
        VariableType::String => "vtString",
        VariableType::UnicodeString => "vtUnicodeString",
        VariableType::ByteArray => "vtByteArray",
        VariableType::Binary => "vtBinary",
        VariableType::AutoAssembler => "vtAutoAssembler",
        VariableType::Pointer => "vtPointer",
        VariableType::Custom(_) => "vtCustom",
        VariableType::Grouped => "vtGrouped",
    };

    let mut actions = json!({
        "toggle": {
            "method": "POST",
            "path": "/api/features/toggle",
            "body": { "id": rec.id, "description": rec.description, "active": !rec.active }
        }
    });

    if !rec.is_group_header && rec.var_type != VariableType::AutoAssembler {
        actions["setValue"] = json!({
            "method": "POST",
            "path": "/api/features/set",
            "body": { "id": rec.id, "description": rec.description, "value": "<NEW_VALUE>" }
        });
    }

    let mut obj = json!({
        "id": rec.id,
        "description": rec.description,
        "type": type_str,
        "active": rec.active,
        "value": val,
        "actions": actions,
    });

    if rec.is_group_header {
        obj["isGroup"] = json!(true);
    }

    // Children are hidden through the tree node, not by omitting them
    // (its `SetVisibleChildrenState` logic):
    //
    //   if ((not factive) and (moHideChildren in options))
    //       or (moAlwaysHideChildren in options) then collapse else expand
    //
    // These two options were parsed and then dropped on the floor here, so a
    // panel showed internals normally kept out of sight - for rdr2.CT that is the
    // whole "BASE" layer, which the startup cascade legitimately turns on. The
    // records really are active either way; you just never see them, so a panel
    // that lists them reads as "everything got enabled behind my back".
    //
    // Sent rather than applied here: the payload stays a faithful description
    // of the table and the panel decides how to render it, exactly as the
    // treeview does.
    if rec.options.hide_children {
        obj["hideChildren"] = json!(true);
    }
    if rec.options.always_hide_children {
        obj["alwaysHideChildren"] = json!(true);
    }

    if let Some(err) = read_error {
        obj["valueError"] = json!(err);
    }

    if !rec.address_expr.is_empty() {
        obj["address"] = json!(rec.address_expr);
    }
    if !rec.offsets.is_empty() {
        obj["offsets"] = json!(rec.offsets);
    }
    if rec.aa_script.as_ref().is_some_and(|s| s.is_lua) {
        obj["luaScript"] = json!(true);
    }

    if !rec.children.is_empty() {
        let child_json: Vec<serde_json::Value> = rec.children.iter().map(|c| record_to_json(c, state)).collect();
        obj["children"] = json!(child_json);
    }

    obj
}

fn build_features_json(state: &RuntimeState) -> serde_json::Value {
    let attached = state.process.is_some();
    let proc_name = state.process.as_ref().map(|p| p.name.as_str()).unwrap_or("");
    let proc_id = state.process.as_ref().map(|p| p.pid).unwrap_or(0);

    let features: Vec<serde_json::Value> = state
        .records
        .iter()
        .map(|rec| record_to_json(rec, state))
        .collect();

    json!({
        "endpoints": {
            "features": { "method": "GET", "path": "/api/features" },
            "toggle": { "method": "POST", "path": "/api/features/toggle" },
            "set_value": { "method": "POST", "path": "/api/features/set" },
            "activate_all": { "method": "POST", "path": "/api/features/activate_all" },
            "deactivate_all": { "method": "POST", "path": "/api/features/deactivate_all" },
            "attach": { "method": "POST", "path": "/api/process/attach" },
            "shutdown": { "method": "POST", "path": "/api/shutdown" }
        },
        "trainer": {
            "table": state.table_name,
            "exe": state.trainer_exe_name
        },
        "process": {
            "attached": attached,
            "processId": proc_id,
            "processName": proc_name,
            "canSelfAttach": true
        },
        "features": features
    })
}

async fn read_body(req: Request<hyper::body::Incoming>) -> Result<Vec<u8>, hyper::Error> {
    use http_body_util::BodyExt;
    let bytes = req.into_body().collect().await?.to_bytes();
    Ok(bytes.to_vec())
}

fn json_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Access-Control-Allow-Origin", "*")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

/// Minimal percent-decoding for debug query parameters.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}


/// Turns off every active script in a subtree, deepest first, so a parent's
/// symbols outlive the children that reference them.
fn deactivate_recursive(
    rec: &mut MemoryRecord,
    process: &mut GameProcess,
    env: &mut ExecEnv<'_>,
    off: &mut Vec<String>,
    failed: &mut Vec<String>,
) {
    for child in &mut rec.children {
        deactivate_recursive(child, process, env, off, failed);
    }
    if rec.active {
        match rec.set_active(false, process, env) {
            Ok(()) => off.push(rec.description.clone()),
            Err(e) => failed.push(format!("{}: {}", rec.description, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case `auto_activate` exists for. RDR2's `onOpenProcess` arms a timer
    /// that asks for record 397906 — the root group whose
    /// `moActivateChildrenAsWell` takes all nine "BASE" scripts up with it,
    /// `Main CodeBase` among them. Nobody pressed anything; the table did it.
    ///
    /// A disable and a value write in the same batch are left alone, because
    /// neither is the table injecting a cheat behind the player's back.
    #[test]
    fn a_tables_own_enable_request_is_held_back() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rdr2.CT");
        let table = crate::ct_parser::parse_file(path).unwrap();
        let records: Vec<MemoryRecord> =
            table.entries.iter().map(MemoryRecord::from_cheat_entry).collect();

        let mut actions = vec![
            LuaAction::SetActive { id: 397906, active: true },
            LuaAction::SetActive { id: 54, active: false },
            LuaAction::SetValue { id: 54, value: "1".to_string() },
        ];

        assert_eq!(hold_back_table_enables(&mut actions, &records), 1);
        assert_eq!(
            actions,
            vec![
                LuaAction::SetActive { id: 54, active: false },
                LuaAction::SetValue { id: 54, value: "1".to_string() },
            ]
        );
    }

    /// The record the log names has to be the one the player would recognise,
    /// so the lookup must recurse: 54 is a child of the root group, not a
    /// top-level entry.
    #[test]
    fn records_are_found_at_any_depth() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rdr2.CT");
        let table = crate::ct_parser::parse_file(path).unwrap();
        let records: Vec<MemoryRecord> =
            table.entries.iter().map(MemoryRecord::from_cheat_entry).collect();

        let found = find_record(&records, 54).expect("Main CodeBase is in the table");
        assert!(found.description.contains("Main CodeBase"));
    }
    /// The cascade the panel depends on is only reachable if the option is
    /// parsed: `Main CodeBase` carries `moActivateChildrenAsWell`, and its one
    /// child is what a user toggling it expects to come up too.
    #[test]
    fn main_codebase_carries_activate_children_as_well() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rdr2.CT");
        let table = crate::ct_parser::parse_file(path).unwrap();
        let records: Vec<MemoryRecord> =
            table.entries.iter().map(MemoryRecord::from_cheat_entry).collect();
        let mc = find_record(&records, 54).expect("Main CodeBase");
        assert_eq!(mc.children.len(), 1, "one child: BASEWelcomeMess");
        assert!(
            mc.options.activate_children_as_well,
            "moActivateChildrenAsWell must survive parsing or nothing cascades"
        );
    }
}
