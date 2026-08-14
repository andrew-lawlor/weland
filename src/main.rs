use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

use weland::compiler::{compile_epub, CompileOptions};
use weland::toolkit::{export_wld, extract_assets, inspect_wld, search_wld, ExportFormat};

#[derive(Parser, Debug)]
#[command(
    name = "weland",
    author = "Andrew",
    version,
    about = "High-performance compiler and toolkit for the Weland (.wld) ebook standard",
    long_about = "Weland compiles EPUB archives into highly optimized SQLite databases containing\n\
                  an AST representation, extracted assets, FTS5 search index, and annotation support."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Direct input EPUB file (shorthand for 'compile <input>')
    #[arg(value_name = "INPUT_EPUB", global = false)]
    direct_input: Option<PathBuf>,

    /// Direct output .wld file (used with direct input shorthand)
    #[arg(value_name = "OUTPUT_WLD", global = false)]
    direct_output: Option<PathBuf>,

    /// Suppress informational logs
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Show detailed verbose logs
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Compile an EPUB file into a Weland (.wld) database
    Compile {
        /// Input .epub file path
        #[arg(value_name = "INPUT")]
        input: PathBuf,

        /// Output .wld / .sqlite database path (defaults to <input_stem>.wld)
        #[arg(value_name = "OUTPUT")]
        output: Option<PathBuf>,
    },

    /// Inspect metadata, AST node breakdown, assets, and schema of a .wld file
    Inspect {
        /// Input .wld database file
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },

    /// Search full-text content in a .wld database using FTS5
    Search {
        /// Input .wld database file
        #[arg(value_name = "FILE")]
        file: PathBuf,

        /// Search query (supports FTS5 syntax, e.g. "phrase query" or term1 AND term2)
        #[arg(value_name = "QUERY")]
        query: String,

        /// Maximum number of search results to return
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },

    /// Extract cover image or all embedded assets from a .wld database
    Extract {
        /// Input .wld database file
        #[arg(value_name = "FILE")]
        file: PathBuf,

        /// Destination directory for extracted assets
        #[arg(short, long, default_value = "./extracted_assets")]
        out_dir: PathBuf,

        /// Extract cover image only
        #[arg(long)]
        cover_only: bool,
    },

    /// Export AST nodes to Markdown, JSON, or Plain Text
    Export {
        /// Input .wld database file
        #[arg(value_name = "FILE")]
        file: PathBuf,

        /// Target export format
        #[arg(short, long, value_enum, default_value_t = CliExportFormat::Markdown)]
        format: CliExportFormat,

        /// Output file path (prints to stdout if omitted)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum CliExportFormat {
    Markdown,
    Json,
    Text,
}

impl From<CliExportFormat> for ExportFormat {
    fn from(f: CliExportFormat) -> Self {
        match f {
            CliExportFormat::Markdown => ExportFormat::Markdown,
            CliExportFormat::Json => ExportFormat::Json,
            CliExportFormat::Text => ExportFormat::Text,
        }
    }
}

fn derive_default_output(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default();
    let mut out = input.to_path_buf();
    out.set_file_name(stem);
    out.set_extension("wld");
    out
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let options = CompileOptions {
        quiet: cli.quiet,
        verbose: cli.verbose,
    };

    match cli.command {
        Some(Commands::Compile { input, output }) => {
            let out = output.unwrap_or_else(|| derive_default_output(&input));
            compile_epub(&input, &out, &options)?;
        }
        Some(Commands::Inspect { file }) => {
            inspect_wld(&file)?;
        }
        Some(Commands::Search { file, query, limit }) => {
            search_wld(&file, &query, limit)?;
        }
        Some(Commands::Extract { file, out_dir, cover_only }) => {
            extract_assets(&file, &out_dir, cover_only)?;
        }
        Some(Commands::Export { file, format, output }) => {
            export_wld(&file, format.into(), output.as_deref())?;
        }
        None => {
            if let Some(input) = cli.direct_input {
                let out = cli.direct_output.unwrap_or_else(|| derive_default_output(&input));
                compile_epub(&input, &out, &options)?;
            } else {
                eprintln!(
                    "Usage:\n  weland compile <input.epub> [output.wld]\n  weland inspect <book.wld>\n  weland search <book.wld> <query>\n  weland extract <book.wld>\n  weland export <book.wld>\n\nRun 'weland --help' for more options."
                );
            }
        }
    }

    Ok(())
}
