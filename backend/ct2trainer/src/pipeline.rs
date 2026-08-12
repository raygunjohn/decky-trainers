use crate::archive::{build_archive, ArchiveFile};
use crate::cetrainer;
use crate::overlay;
use anyhow::{bail, Context, Result};
use flate2::Compression;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;

pub struct GeneratorConfig {
    pub input_table: PathBuf,
    pub output_exe: PathBuf,
    pub runtime_dir: PathBuf,
    pub compression_level: Compression,
}

/// The runtime images this generator will embed, best name first.
///
/// `deckytrainers-x86_64.exe` is what `entrypoint.sh` stages and therefore
/// what a plugin build provides; `cheatengine-x86_64.exe` is the legacy name
/// kept so an existing `--runtime-dir` still resolves; `trainer-runtime.exe` is
/// what a plain `cargo build --target x86_64-pc-windows-gnu` leaves in
/// `target/x86_64-pc-windows-gnu/release`, so a normal build tree works as a
/// `--runtime-dir` with no staging step.
const RUNTIME_CANDIDATES: [&str; 3] = [
    "deckytrainers-x86_64.exe",
    "cheatengine-x86_64.exe",
    "trainer-runtime.exe",
];

/// Builds the `.CETRAINER` bytes for `config.input_table`: already-protected
/// tables pass through unchanged, everything else gets the 3-pass XOR.
fn build_cetrainer_bytes(config: &GeneratorConfig) -> Result<Vec<u8>> {
    info!("Loading table: {:?}", config.input_table);
    let raw_table_bytes = fs::read(&config.input_table)
        .with_context(|| format!("Failed to read input table: {:?}", config.input_table))?;

    if raw_table_bytes.starts_with(b"CHEAT") {
        Ok(raw_table_bytes)
    } else {
        cetrainer::protect(&raw_table_bytes).context("Failed to protect .CETRAINER binary")
    }
}

/// Packages `config.input_table` into a standalone Windows trainer.
///
/// The output is the runtime PE verbatim with a DEFLATE archive and a footer
/// appended past its last section - see `overlay.rs`. There is one packaging
/// path on purpose: the trainer runs under Proton, so a PE is the only thing
/// worth emitting, and a generator with a single path cannot pick the wrong
/// one.
pub fn generate_trainer(config: &GeneratorConfig) -> Result<()> {
    let cetrainer_bytes = build_cetrainer_bytes(config)?;

    let mut files = vec![ArchiveFile {
        filename: "HTT_TRAINER.CETRAINER".to_string(),
        folder: "".to_string(),
        data: cetrainer_bytes,
    }];

    // Absent is not an error: the runtime writes a usable defines.lua beside
    // itself at startup, and a table that needs the real one will say so.
    let defines_path = config.runtime_dir.join("defines.lua");
    let defines_data =
        fs::read(&defines_path).unwrap_or_else(|_| b"-- minimal defines.lua\n".to_vec());
    files.push(ArchiveFile {
        filename: "defines.lua".to_string(),
        folder: "".to_string(),
        data: defines_data,
    });

    let archive_bytes =
        build_archive(&files, config.compression_level).context("Failed to build archive stream")?;

    let runtime_bytes = load_runtime(&config.runtime_dir)?;

    // Create the output directory if needed, so `--output dist/x.exe` works on
    // a fresh clone rather than failing with a bare "path not found".
    if let Some(parent) = config.output_exe.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create output directory: {:?}", parent))?;
        }
    }

    let out_bytes = overlay::append_overlay(&runtime_bytes, &archive_bytes);
    fs::write(&config.output_exe, &out_bytes)
        .with_context(|| format!("Failed to write output executable: {:?}", config.output_exe))?;

    // Wine does not need the executable bit to run a PE, but the plugin's
    // launch path checks it before spawning, and a trainer that lists but
    // will not start is a worse failure than one that never lists.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&config.output_exe)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(&config.output_exe, perms)?;
    }

    info!(
        "Trainer successfully generated: {:?} ({} bytes runtime + {} bytes table)",
        config.output_exe,
        runtime_bytes.len(),
        archive_bytes.len()
    );
    info!(
        "Run it under the same Proton prefix as the game - a trainer in its own \
         wineserver session cannot see the game's processes."
    );

    Ok(())
}

/// Finds the Windows runtime PE to use as the base image.
///
/// A candidate that exists but is not an x86-64 PE is a hard error naming what
/// it actually is, rather than something skipped on the way to a worse message
/// later. The overwhelmingly likely case is the native Linux `trainer-runtime`
/// sitting in the same directory as the cross-compiled one, which would
/// otherwise produce an ELF with a PE's name and fail only on the Deck.
fn load_runtime(runtime_dir: &Path) -> Result<Vec<u8>> {
    for name in RUNTIME_CANDIDATES {
        let path = runtime_dir.join(name);
        if !path.exists() {
            continue;
        }
        let bytes =
            fs::read(&path).with_context(|| format!("Failed to read runtime: {:?}", path))?;
        if let Some(why) = overlay::reject_as_runtime(&bytes) {
            bail!(
                "{:?} cannot be used as a trainer runtime: {}. Build one with \
                 `cargo build --release -p trainer-runtime --target x86_64-pc-windows-gnu` \
                 (see the Dockerfile for the toolchain), or point --runtime-dir at a directory \
                 that has one.",
                path,
                why
            );
        }
        info!("Embedding runtime: {:?} ({} bytes)", path, bytes.len());
        return Ok(bytes);
    }

    bail!(
        "No Windows runtime found in {:?}. Expected one of: {}. Build one with \
         `cargo build --release -p trainer-runtime --target x86_64-pc-windows-gnu`, or \
         point --runtime-dir at a directory that has one.",
        runtime_dir,
        RUNTIME_CANDIDATES.join(", ")
    )
}
