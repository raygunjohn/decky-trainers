//! Periodic runtime tick: value freezing, auto-attach, and the Lua scheduler.

use crate::lua_engine::LuaCtx;
use crate::memory_record::MemoryRecord;
use crate::process::GameProcess;
use crate::rest_server::{apply_lua_actions, collect_record_views, on_process_opened, SharedState};
use std::thread;
use std::time::Duration;
use tracing::{info, warn};

/// How often the loop runs. Also the resolution of Lua `sleep()` inside a
/// `createThread` body, so keep it short.
const TICK_MS: u64 = 50;

pub fn start_freeze_loop(state: SharedState) {
    thread::spawn(move || {
        let mut since_attach_try = 0u64;
        let mut reported_attach_error = false;

        loop {
            thread::sleep(Duration::from_millis(TICK_MS));

            let mut guard = match state.lock() {
                Ok(g) => g,
                Err(_) => continue,
            };
            let state_ref = &mut *guard;

            // Drop a dead target. Everything registered against it - symbols,
            // active flags, injected addresses - refers to memory that no longer
            // exists, and leaving `process` set makes the API report a healthy
            // attachment while every read fails.
            if state_ref.process.as_ref().is_some_and(|p| !p.is_alive()) {
                let name = state_ref
                    .process
                    .as_ref()
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                warn!("{} exited; detaching and clearing script state", name);
                state_ref.process = None;
                state_ref.symbols = crate::symbol_handler::SymbolHandler::new();
                clear_active(&mut state_ref.records);
                state_ref.pending_activations.clear();
                reported_attach_error = false;
            }

            // Reattach if the game was started (or restarted) after the trainer.
            since_attach_try += TICK_MS;
            if state_ref.process.is_none() && since_attach_try >= 1000 {
                since_attach_try = 0;
                if let Some(target) = state_ref.auto_attach_target.clone() {
                    match GameProcess::attach_by_name(&target) {
                        Ok(p) => {
                            info!("Attached to {} (pid {})", p.name, p.pid);
                            state_ref.process = Some(p);
                            reported_attach_error = false;
                            // `onOpenProcess` fires from the process-handle
                            // setter, so it runs for an auto-attach exactly as
                            // it does for a manual one.
                            on_process_opened(state_ref);
                        }
                        Err(e) => {
                            // Log once, not every second.
                            if !reported_attach_error {
                                warn!("Auto-attach to '{}' failed: {}", target, e);
                                reported_attach_error = true;
                            }
                        }
                    }
                }
            }

            // Re-apply frozen values.
            if let Some(ref proc) = state_ref.process {
                freeze_recursive(&state_ref.records, proc, &state_ref.symbols);
            }

            // Drive Lua threads and timers created by createThread/createTimer.
            let views = collect_record_views(&state_ref.records);
            let mut pending = Vec::new();
            {
                let mut ctx = LuaCtx {
                    process: state_ref.process.as_mut(),
                    symbols: &mut state_ref.symbols,
                    records: views,
                    actions: Vec::new(),
                };
                if let Err(e) = state_ref.lua.tick(&mut ctx) {
                    warn!("lua tick failed: {}", e);
                }
                pending = std::mem::take(&mut ctx.actions);
            }

            // A timer or coroutine asking to enable a record is the table
            // acting on its own; RDR2's whole startup cascade arrives here, one
            // second after attach. See `RuntimeState::auto_activate`.
            if !state_ref.auto_activate {
                crate::rest_server::hold_back_table_enables(&mut pending, &state_ref.records);
            }

            let mut deferred = Vec::new();
            if !pending.is_empty() {
                if let Some(ref mut proc) = state_ref.process {
                    apply_lua_actions(
                        pending,
                        &mut state_ref.records,
                        proc,
                        &mut state_ref.symbols,
                        &state_ref.lua,
                        &mut deferred,
                    );
                }
            }
            state_ref.pending_activations.extend(deferred);

            // Drain the whole cascade in this tick, depth-first - the shape.
            //
            // Once a record's own script is done, the cascade loops over the tree
            // calling `setActive(true)` on each child, and each of those
            // recurses the same way. That is one synchronous depth-first
            // recursion with no yield anywhere; only a record carrying
            // `Async="1"` gets a processing thread, and rdr2.CT has none.
            // So a nine-script
            // cascade comes up back-to-back, and such tables expect
            // its whole BASE layer to exist by the time the first one returns.
            //
            // This used to activate one record per 50 ms tick, which reordered
            // the cascade breadth-first and spread it over half a second. That
            // pacing was introduced for the ptrace backend, where every
            // allocation stopped the game and nine scripts back to back killed
            // it. `VirtualAllocEx` stops nothing, so the reason is gone with
            // the backend.
            //
            // The queue itself stays: it is what keeps the AA engine from being
            // re-entered mid-script, which is a different concern from pacing.
            // Draining it here iteratively is the same order the recursion
            // produces, without the stack depth.
            //
            // Children go to the *front*, in order, so a parent's subtree runs
            // before the parent's siblings.
            if state_ref.process.is_some() && !state_ref.pending_activations.is_empty() {
                // A tree cannot cycle, but a hand-edited table can name itself
                // a child; naive recursion would overflow the stack. Cap
                // the drain instead, and say so rather than hanging the tick.
                let budget = count_records(&state_ref.records).saturating_mul(2).max(64);
                let mut done = 0usize;

                while let Some((id, active)) = state_ref.pending_activations.pop_front() {
                    done += 1;
                    if done > budget {
                        warn!(
                            "activation cascade exceeded {} steps - stopping. A record is \
                             most likely listed as its own descendant.",
                            budget
                        );
                        state_ref.pending_activations.clear();
                        break;
                    }

                    let Some(ref mut proc) = state_ref.process else { break };
                    let views = collect_record_views(&state_ref.records);
                    let mut more = Vec::new();
                    {
                        let mut env = crate::lua_engine::ExecEnv::new(
                            &mut state_ref.symbols,
                            &state_ref.lua,
                            &views,
                        );
                        if let Some(rec) = find_record_mut(&mut state_ref.records, id) {
                            let desc = rec.description.clone();
                            if let Err(e) = rec.set_active(active, proc, &mut env) {
                                warn!("queued activation of '{}' failed: {}", desc, e);
                            }
                        }
                        more.append(&mut env.deferred_activations);
                    }
                    for entry in more.into_iter().rev() {
                        state_ref.pending_activations.push_front(entry);
                    }
                }
            }
        }
    });
}

/// Freezing has to walk the whole tree: children of a group carry their own
/// frozen values.
fn freeze_recursive(
    records: &[MemoryRecord],
    proc: &GameProcess,
    symbols: &crate::symbol_handler::SymbolHandler,
) {
    for rec in records {
        if rec.active {
            if let Some(ref val) = rec.frozen_value {
                let _ = rec.write_value(proc, symbols, val);
            }
        }
        freeze_recursive(&rec.children, proc, symbols);
    }
}

fn find_record_mut(records: &mut [MemoryRecord], id: u32) -> Option<&mut MemoryRecord> {
    for rec in records {
        if rec.id == id {
            return Some(rec);
        }
        if let Some(f) = find_record_mut(&mut rec.children, id) {
            return Some(f);
        }
    }
    None
}

/// Marks every record inactive after the target dies, so a later attach starts
/// from a clean slate instead of believing scripts are still injected.
fn clear_active(records: &mut [MemoryRecord]) {
    for rec in records {
        rec.active = false;
        rec.frozen_value = None;
        clear_active(&mut rec.children);
    }
}

/// Total records in the tree, used to bound one cascade drain.
fn count_records(records: &[MemoryRecord]) -> usize {
    records
        .iter()
        .map(|r| 1 + count_records(&r.children))
        .sum()
}
