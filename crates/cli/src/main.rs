use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

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
        [path] if !path.to_string_lossy().starts_with('-') => {
            match skarmory_core::analyze(Path::new(path)) {
                Err(error) => eprintln!("skarmory: {error}"),
                Ok(()) => eprintln!("skarmory: no analysis report was produced"),
            }
            ExitCode::from(2)
        }
        _ => {
            eprintln!("skarmory: unsupported arguments; use --help for scaffold commands");
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "Skarmory {}\nFast {} code analysis, powered by Rust.\n\n\
         Status: development scaffold; analysis is not implemented.\n\n\
         Usage: skarmory [PATH | --help | --version]\n\n\
         --help       Show this message\n\
         --version    Show the development version\n\
         PATH         Request analysis (currently reports unavailable; exit 2)",
        skarmory_core::VERSION,
        skarmory_python::LANGUAGE,
    );
}
