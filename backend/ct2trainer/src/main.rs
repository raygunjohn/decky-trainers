mod archive;
mod cetrainer;
mod overlay;
mod pipeline;

use anyhow::Result;
use clap::Parser;
use flate2::Compression;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// Generates a standalone Windows trainer from a cheat table.
///
/// This binary is a Linux ELF that emits a Windows PE: the trainer runs under
/// the game's Proton prefix, where `ReadProcessMemory`, `VirtualAllocEx` and
/// `CreateRemoteThread` are the same primitives such tools use.
/// There is no Linux-target output and no `--target` flag - see CLAUDE.md.
#[derive(Parser, Debug)]
#[command(author, version, about = "DeckyTrainers - headless trainer generator")]
struct Cli {
    /// Input Cheat Table (.CT) file
    #[arg(short, long)]
    input: PathBuf,

    /// Output trainer executable (.exe, run under Wine/Proton)
    #[arg(short, long)]
    output: PathBuf,

    /// Target bitness. Only 64 is supported: the assembler is an x86-64
    /// encoder and no 32-bit runtime is built, so this exists to reject a
    /// stale `--bits 32` loudly rather than to select anything.
    #[arg(short, long, default_value_t = 64)]
    bits: u32,

    /// Directory holding the runtime PE to embed (and optionally defines.lua)
    #[arg(long, default_value = ".")]
    runtime_dir: PathBuf,

    /// Compression level: none, fastest, default, max
    #[arg(long, default_value = "max")]
    compression: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    if cli.bits != 64 {
        anyhow::bail!(
            "--bits {} is not supported: the assembler is an x86-64 encoder and no 32-bit \
             runtime is built. Drop the flag or pass --bits 64.",
            cli.bits
        );
    }

    let comp_level = match cli.compression.to_lowercase().as_str() {
        "none" => Compression::none(),
        "fastest" => Compression::fast(),
        "default" => Compression::default(),
        "max" => Compression::best(),
        _ => Compression::best(),
    };

    let config = pipeline::GeneratorConfig {
        input_table: cli.input,
        output_exe: cli.output,
        runtime_dir: cli.runtime_dir,
        compression_level: comp_level,
    };

    pipeline::generate_trainer(&config)?;

    Ok(())
}
