use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

fn main() -> ExitCode {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    match arguments.as_slice() {
        [] => {
            print_help();
            ExitCode::SUCCESS
        }
        [flag] if flag == "--help" || flag == "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("skarmory {}", skarmory_core::VERSION);
            ExitCode::SUCCESS
        }
        [path] if !path.to_string_lossy().starts_with('-') => scan(Path::new(path), false),
        [flag, path] if flag == "--timings" => scan(Path::new(path), true),
        _ => {
            eprintln!("skarmory: unsupported arguments; use --help");
            ExitCode::from(2)
        }
    }
}

fn scan(path: &Path, print_timings: bool) -> ExitCode {
    let (report, timings) = skarmory_core::analyze_with_timings(path);
    let start = Instant::now();
    let result = (|| {
        let json = report.to_json()?;
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{json}")?;
        stdout.flush()
    })();
    if let Err(error) = result {
        eprintln!("skarmory: failed to write report: {error}");
        return ExitCode::from(2);
    }
    if print_timings {
        eprintln!(
            "{}",
            serde_json::json!({ "analysis": timings, "output_ns": start.elapsed().as_nanos() })
        );
    }
    if report.is_complete() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn print_help() {
    println!(
        "Skarmory {}\nPhase 0 Python dead-code prototype.\n\n\
         Usage: skarmory [--timings] PATH\n\n\
         --help       Show this message\n\
         --version    Show the development version\n\
         --timings    Write phase timings as JSON to stderr\n\
         PATH         Analyze one .py file or a flat directory rooted at main.py\n\n\
         Reports are written to stdout as JSON. Incomplete analysis exits with 2.",
        skarmory_core::VERSION,
    );
}
