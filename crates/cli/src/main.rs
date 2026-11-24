use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Args, CommandFactory, Parser, ValueEnum};
use skarmory_core::phase02::{self, workflow};
use skarmory_core::{CacheStats, ScanOptions, ScanPolicy, Timings};

#[derive(Clone, ValueEnum)]
enum Profile {
    Native,
    Skylos,
    #[value(name = "skylos-0.3")]
    SkylosPhase03,
}

#[derive(Clone, ValueEnum)]
enum Format {
    Json,
    JsonCi,
}

#[derive(Clone, ValueEnum)]
enum DiffMode {
    Lines,
    Impact,
}

#[derive(Clone, ValueEnum)]
enum Policy {
    Application,
    Library,
}

#[derive(Parser)]
#[command(name = "skarmory", version = skarmory_core::VERSION)]
#[command(about = "Analyze source code for dead code.")]
#[command(after_help = "JSON goes to stdout by default. Incomplete analysis exits with 2.")]
struct Cli {
    /// Python source file or project directory.
    #[arg(value_name = "PATH")]
    path: PathBuf,

    /// Choose the native preview or a versioned Skylos compatibility profile.
    #[arg(long, value_enum)]
    profile: Option<Profile>,

    #[command(flatten, next_help_heading = "Native preview options")]
    preview: Option<PreviewArgs>,

    #[command(flatten, next_help_heading = "Skylos compatibility options")]
    skylos: Option<SkylosArgs>,

    /// Maximum number of workers for file processing.
    #[arg(long, default_value_t = 1)]
    workers: usize,

    /// Reuse validated file-local facts from this directory.
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Write native phase timings and optional cache metrics as JSON to stderr.
    #[arg(long)]
    timings: bool,

    /// Scan with the released Phase 0 flat-project contract.
    #[arg(
        long,
        conflicts_with_all = ["preview_options", "skylos_options", "workers", "cache_dir", "profile"],
    )]
    phase0: bool,
}

#[derive(Args, Default)]
#[group(id = "preview_options")]
struct PreviewArgs {
    /// Declare a source root. Repeat to set the import search order.
    #[arg(long = "source-root", value_name = "DIR")]
    source_roots: Vec<PathBuf>,

    /// Select application entries or a library's public API as analysis roots.
    #[arg(long, value_enum)]
    policy: Option<Policy>,

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

    /// Search selected sources and supported configuration for possible use.
    #[arg(long)]
    grep_verify: bool,
}

#[derive(Args, Default)]
#[group(id = "skylos_options")]
struct SkylosArgs {
    /// Replace discovered Skylos settings with this TOML file.
    #[arg(long, value_name = "PATH")]
    config_file: Option<PathBuf>,

    /// Exclude a source folder or pattern. Repeat as needed.
    #[arg(long = "exclude", alias = "exclude-folder", value_name = "PATTERN")]
    excludes: Vec<String>,

    /// Include an otherwise excluded folder. Repeat as needed.
    #[arg(long = "include-folder", value_name = "PATTERN")]
    includes: Vec<String>,

    /// Turn off the default source exclusions.
    #[arg(long)]
    no_default_excludes: bool,

    /// Also scan generated source files.
    #[arg(long)]
    include_generated: bool,

    /// Minimum finding confidence from 0 through 100.
    #[arg(short = 'c', long, value_parser = clap::value_parser!(u8).range(0..=100))]
    confidence: Option<u8>,

    /// Select the declared dead_code category.
    #[arg(long, value_name = "CATEGORY")]
    category: Option<String>,

    /// Include optional advice in the compatibility result.
    #[arg(long)]
    all_advice: bool,

    /// Enable the Phase 0.3 Python unused-dependency quality rule.
    #[arg(long)]
    quality: bool,

    /// Opt into experimental JavaScript and TypeScript analysis of a package directory.
    #[arg(long)]
    experimental_javascript: bool,

    /// Select Phase 0.3 rule IDs. Repeat or separate multiple IDs with commas.
    #[arg(long, value_delimiter = ',', value_name = "RULE")]
    select: Vec<String>,

    /// Set the minimum displayed finding severity in Phase 0.3.
    #[arg(long, value_name = "LEVEL")]
    severity: Option<String>,

    /// Report findings on working-tree lines changed from the Git merge base.
    #[arg(long, value_name = "REF", num_args = 0..=1, default_missing_value = "auto", conflicts_with = "diff_base")]
    diff: Option<String>,

    /// Report findings in working-tree files changed from the Git merge base.
    #[arg(long, value_name = "REF", conflicts_with = "diff_mode")]
    diff_base: Option<String>,

    /// Include newly unused symbols anywhere in the project in impact mode.
    #[arg(long, value_enum, requires = "diff")]
    diff_mode: Option<DiffMode>,

    /// Suppress existing dead code from a versioned baseline.
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = ".skylos/baseline.json", conflicts_with = "baseline_ref")]
    baseline: Option<PathBuf>,

    /// Use the project baseline committed at this Git revision.
    #[arg(long, value_name = "REF")]
    baseline_ref: Option<String>,

    /// Record dead-code findings from a complete, unfiltered scan.
    #[arg(long, value_name = "PATH")]
    write_baseline: Option<PathBuf>,

    /// Evaluate the configured Phase 0.3 dead-code gate.
    #[arg(long)]
    gate: bool,

    /// Require complete Phase 0.3 coverage for the gate.
    #[arg(long)]
    require_complete: bool,

    /// Return success for an evaluated gate failure while retaining its verdict.
    #[arg(long, requires = "gate")]
    force: bool,

    /// Disable grep verification in the Skylos compatibility profile.
    #[arg(long = "no-grep-verify")]
    no_grep_verify: bool,

    /// Read a runtime trace in the Phase 0.3 profile. Defaults to .skylos_trace.
    #[arg(long, value_name = "PATH", conflicts_with = "no_trace")]
    trace_file: Option<PathBuf>,

    /// Disable implicit trace discovery in the Phase 0.3 profile.
    #[arg(long)]
    no_trace: bool,

    /// Read runtime line coverage in the Phase 0.3 profile. Defaults to .coverage.
    #[arg(long, value_name = "PATH", conflicts_with = "no_coverage")]
    coverage_file: Option<PathBuf>,

    /// Disable implicit runtime coverage discovery in the Phase 0.3 profile.
    #[arg(long)]
    no_coverage: bool,

    /// Write the legacy JSON report (the compatibility default).
    #[arg(long = "json")]
    _json: bool,

    /// Select the legacy machine-readable format.
    #[arg(long = "format", value_enum)]
    format: Option<Format>,

    /// Exit with 1 when complete analysis reports findings.
    #[arg(long)]
    strict: bool,

    /// Write JSON to this file instead of stdout.
    #[arg(short = 'o', long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Also write a SARIF 2.1.0 report. Defaults to skylos.sarif.json.
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "skylos.sarif.json")]
    sarif: Option<PathBuf>,
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
        return write_report(report.is_complete(), timings, cli.timings, None, || {
            report.to_json()
        });
    }
    let compatibility_profile = match cli.profile {
        Some(Profile::Skylos) => Some(phase02::Profile::SkylosPhase02),
        Some(Profile::SkylosPhase03) => Some(phase02::Profile::SkylosPhase03),
        Some(Profile::Native) | None => None,
    };
    if let Some(profile) = compatibility_profile {
        if cli.preview.is_some() || cli.timings {
            return fail("native preview options and --timings require the native profile");
        }
        return write_skylos_report(
            &cli.path,
            profile,
            cli.skylos.unwrap_or_default(),
            cli.workers,
            cli.cache_dir,
        );
    }
    if cli.skylos.is_some() {
        return fail(
            "Skylos compatibility options require --profile skylos or --profile skylos-0.3",
        );
    }

    let preview = cli.preview.unwrap_or_default();
    let options = ScanOptions {
        source_roots: preview.source_roots,
        policy: match preview.policy {
            Some(Policy::Library) => ScanPolicy::Library,
            _ => ScanPolicy::Application,
        },
        entry_file: preview.entry_file,
        entry_modules: preview.entry_modules,
        entry_callables: preview.entry_callables,
        public_modules: preview.public_modules,
        public_callables: preview.public_callables,
        workers: cli.workers,
        grep_verify: preview.grep_verify,
    };
    let (report, timings, cache) = if let Some(directory) = cli.cache_dir {
        let (report, timings, stats) = skarmory_core::scan_cached(&cli.path, &options, &directory);
        (report, timings, Some(stats))
    } else {
        let (report, timings) = skarmory_core::scan(&cli.path, &options);
        (report, timings, None)
    };
    write_report(report.is_complete(), timings, cli.timings, cache, || {
        report.to_json()
    })
}

fn write_skylos_report(
    path: &Path,
    profile: phase02::Profile,
    args: SkylosArgs,
    workers: usize,
    cache_dir: Option<PathBuf>,
) -> ExitCode {
    if let (Some(json), Some(sarif)) = (&args.output, &args.sarif)
        && output_identity(json) == output_identity(sarif)
    {
        return fail("JSON and SARIF require different output paths");
    }
    let options = phase02::Options {
        profile,
        experimental_javascript: args.experimental_javascript,
        config_file: args.config_file,
        excludes: args.excludes,
        generated_keep_paths: args.includes.clone(),
        includes: args.includes,
        no_default_excludes: args.no_default_excludes,
        include_generated: (args.include_generated || args.no_default_excludes).then_some(true),
        confidence: args.confidence,
        category: args.category,
        all_advice: args.all_advice,
        enable_quality: args.quality.then_some(true),
        quality_only: false,
        workers,
        cache_dir,
        grep_verify: args.no_grep_verify.then_some(false),
        trace_file: if args.no_trace {
            phase02::EvidenceInput::Disabled
        } else {
            args.trace_file.map_or(
                phase02::EvidenceInput::Default,
                phase02::EvidenceInput::Path,
            )
        },
        coverage_file: if args.no_coverage {
            phase02::EvidenceInput::Disabled
        } else {
            args.coverage_file.map_or(
                phase02::EvidenceInput::Default,
                phase02::EvidenceInput::Path,
            )
        },
    };
    let compact = matches!(args.format, Some(Format::JsonCi));
    if profile == phase02::Profile::SkylosPhase02
        && (compact
            || args.diff.is_some()
            || args.diff_base.is_some()
            || args.baseline.is_some()
            || args.baseline_ref.is_some()
            || args.write_baseline.is_some()
            || !args.select.is_empty()
            || args.severity.is_some()
            || args.gate
            || args.require_complete)
    {
        return fail("dead-code workflow options require --profile skylos-0.3");
    }
    if let Some(baseline) = &args.write_baseline
        && [args.output.as_ref(), args.sarif.as_ref()]
            .into_iter()
            .flatten()
            .any(|path| output_identity(path) == output_identity(baseline))
    {
        return fail("baseline and reports require different output paths");
    }
    let (mut report, exit_code) = if profile == phase02::Profile::SkylosPhase03 {
        let severity = match args
            .severity
            .as_deref()
            .map(workflow::Severity::parse)
            .transpose()
        {
            Ok(severity) => severity,
            Err(error) => return fail(error),
        };
        let diff_mode = match (args.diff_base.is_some(), args.diff_mode) {
            (true, _) => workflow::DiffMode::Files,
            (_, Some(DiffMode::Impact)) => workflow::DiffMode::Impact,
            _ => workflow::DiffMode::Lines,
        };
        let baseline = args
            .baseline
            .map(workflow::BaselineInput::File)
            .or_else(|| args.baseline_ref.map(workflow::BaselineInput::Revision));
        let settings = workflow::Settings {
            diff: args.diff.or(args.diff_base),
            diff_mode,
            baseline,
            write_baseline: args.write_baseline,
            rules: args.select,
            severity,
            gate: args.gate,
            strict: args.strict,
            require_complete: args.require_complete,
            force: args.force,
        };
        match workflow::run(path, &options, &settings) {
            Ok(outcome) => (outcome.report, outcome.exit_code),
            Err(error) => return fail(error),
        }
    } else {
        let report = match phase02::scan(path, &options) {
            Ok(report) => report,
            Err(error) => return fail(error),
        };
        let exit_code = if !report.is_complete() {
            2
        } else {
            u8::from(args.strict && report.has_findings())
        };
        (report, exit_code)
    };
    let result = (|| {
        let json = if compact {
            report.to_json_ci()?
        } else if args.strict && profile == phase02::Profile::SkylosPhase02 {
            report.to_json_strict()?
        } else {
            report.to_json()?
        };
        if let Some(sarif) = args.sarif {
            write_output(&sarif, &report.to_sarif()?)?;
        }
        if let Some(path) = args.output {
            write_output(&path, &json)?;
            if args.strict || args.gate || args.require_complete {
                write_gate_verdict(&json)?;
            }
        } else {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{json}")?;
            stdout.flush()?;
            if args.gate && !report.is_complete() {
                write_gate_verdict(&json)?;
            }
        }
        Ok::<_, io::Error>(())
    })();
    if let Err(error) = result {
        return fail(format_args!("failed to write report: {error}"));
    }
    ExitCode::from(exit_code)
}

fn write_gate_verdict(report: &str) -> io::Result<()> {
    let report: serde_json::Value = serde_json::from_str(report)?;
    let gate = &report["gate"];
    let mut stderr = io::stderr().lock();
    if let Some(headline) = gate["headline"].as_str() {
        writeln!(stderr, "{headline}")?;
    }
    if let Some(reasons) = gate["reason_details"].as_array() {
        for reason in reasons {
            if let Some(message) = reason["message"].as_str() {
                writeln!(stderr, "   • {message}")?;
            }
            if let Some(details) = reason["details"].as_array() {
                for detail in details.iter().filter_map(serde_json::Value::as_str) {
                    writeln!(stderr, "       {detail}")?;
                }
            }
            if let Some(hint) = reason["hint"].as_str() {
                writeln!(stderr, "       {hint}")?;
            }
        }
    }
    if let Some(hint) = gate["hint"].as_str() {
        writeln!(stderr, "   {hint}")?;
    }
    if gate["coverage_status"] == "partial" && gate["verdict"] != "incomplete" {
        for warning in gate["coverage_warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
        {
            writeln!(stderr, "   • {warning}")?;
        }
    }
    stderr.flush()
}

fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn output_identity(path: &Path) -> PathBuf {
    fs::canonicalize(output_parent(path))
        .ok()
        .zip(path.file_name())
        .map_or_else(|| path.to_owned(), |(parent, name)| parent.join(name))
}

fn write_output(path: &Path, contents: &str) -> io::Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a symbolic link", path.display()),
        ));
    }
    let mut file = tempfile::NamedTempFile::new_in(output_parent(path))?;
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "skarmory: {error}");
    ExitCode::from(2)
}

fn write_report(
    complete: bool,
    timings: Timings,
    print_timings: bool,
    cache: Option<CacheStats>,
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
    if print_timings {
        let mut measurements =
            serde_json::json!({ "analysis": timings, "output_ns": start.elapsed().as_nanos() });
        if let Some(cache) = cache {
            measurements["cache"] = serde_json::json!(cache);
        }
        if writeln!(io::stderr().lock(), "{measurements}").is_err() {
            return ExitCode::from(2);
        }
    }
    if complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}
