//! Fallback for every build that is not the shipped Windows one — Linux and
//! macOS development hosts.
//!
//! A trainer attaches as a Windows process inside the game's Proton prefix, so
//! `win32.rs` is the only backend that ever runs in anger. This exists so the
//! crate still *builds* natively, which is what makes `--analyze`,
//! `--verify-asm` and `--check-lua` usable on a dev machine and on the Deck
//! without Wine: those modes parse tables and assemble, and never touch a
//! target.

use anyhow::{bail, Result};

use super::Attached;

pub fn attach_by_name(process_name: &str) -> Result<Attached> {
    bail!(
        "Cannot attach to '{}': this is a {} build of the runtime, which has no process \
         backend. A trainer must be the cross-compiled Windows binary and must run under \
         the same Proton prefix as the game. Offline modes (--analyze, --verify-asm, \
         --check-lua) work in this build.",
        process_name,
        std::env::consts::OS
    )
}
