use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{CommandFactory, Parser, ValueEnum};
use skarmory_core::{ScanOptions, ScanPolicy, Timings};

#[derive(Clone, ValueEnum)]
enum Policy {
    Application,
    Library,
}

#[derive(Parser)]
#[command(name = "skarmory", version = skarmory_core::VERSION)]
#[command(about = "Find unreachable functions in a closed set of Python 3.11 sources.")]
#[command(after_help = "Reports are written to stdout as JSON. Incomplete analysis exits with 2.")]
struct Cli {
    /// Python source file or project directory.
    #[arg(value_name = "PATH")]
    path: PathBuf,

    /// Declare a source root. Repeat to set the import search order.
    #[arg(long = "source-root", value_name = "DIR")]
    source_roots: Vec<PathBuf>,

    /// Select application entries or a library's public API as analysis roots.
    #[arg(long, value_enum, default_value = "application")]
    policy: Policy,

    /// Declare a directly executed file.
    #[arg(long, value_name = "PATH")]
    entry_file: Option<PathBuf>,

    /// Declare a module executed with Python's -m semantics. Repeat as needed.
    #[arg(long = "entry-module", value_name = "MODULE")]
    entry_modules: Vec<String>,

    /// Declare an application entry callable. Repeat as needed.
    #[arg(long = "entry-callable", value_name = "MODULE:FUNCTION")]
    entry_callables: Vec<String>,

    /// Declare a library module exposed to callers. Repeat as needed.
    #[arg(long = "public-module", value_name = "MODULE")]
    public_modules: Vec<String>,

    /// Declare a library callable exposed to callers. Repeat as needed.
    #[arg(long = "public-callable", value_name = "MODULE:FUNCTION")]
    public_callables: Vec<String>,

    /// Maximum number of workers for file processing.
    #[arg(long, default_value_t = 1)]
    workers: usize,

    /// Write phase timings as JSON to stderr.
    #[arg(long)]
    timings: bool,

    /// Scan with the released Phase 0 flat-project contract.
    #[arg(
        long,
        conflicts_with_all = [
            "source_roots", "policy", "entry_file", "entry_modules", "entry_callables",
            "public_modules", "public_callables", "workers"
        ],
    )]
    phase0: bool,
}

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.len() == 1 {
        return match Cli::command().print_help() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                let _ = writeln!(
                    io::stderr().lock(),
                    "skarmory: failed to write help: {error}"
                );
                ExitCode::from(2)
            }
        };
    }
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            return match error.print() {
                Ok(()) if error.exit_code() == 0 => ExitCode::SUCCESS,
                Ok(()) | Err(_) => ExitCode::from(2),
            };
        }
    };
    if cli.phase0 {
        let (report, timings) = skarmory_core::analyze_with_timings(&cli.path);
        return write_report(report.is_complete(), timings, cli.timings, || {
            report.to_json()
        });
    }

    let options = ScanOptions {
        source_roots: cli.source_roots,
        policy: match cli.policy {
            Policy::Application => ScanPolicy::Application,
            Policy::Library => ScanPolicy::Library,
        },
        entry_file: cli.entry_file,
        entry_modules: cli.entry_modules,
        entry_callables: cli.entry_callables,
        public_modules: cli.public_modules,
        public_callables: cli.public_callables,
        workers: cli.workers,
    };
    let (report, timings) = skarmory_core::scan(&cli.path, &options);
    write_report(report.is_complete(), timings, cli.timings, || {
        report.to_json()
    })
}

fn write_report(
    complete: bool,
    timings: Timings,
    print_timings: bool,
    serialize: impl FnOnce() -> serde_json::Result<String>,
) -> ExitCode {
    let start = Instant::now();
    let result = (|| {
        let json = serialize()?;
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{json}")?;
        stdout.flush()
    })();
    if let Err(error) = result {
        let _ = writeln!(
            io::stderr().lock(),
            "skarmory: failed to write report: {error}"
        );
        return ExitCode::from(2);
    }
    if print_timings
        && writeln!(
            io::stderr().lock(),
            "{}",
            serde_json::json!({ "analysis": timings, "output_ns": start.elapsed().as_nanos() })
        )
        .is_err()
    {
        return ExitCode::from(2);
    }
    if complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}
