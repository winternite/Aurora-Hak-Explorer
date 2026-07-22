#![forbid(unsafe_code)]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, anyhow, bail};
use aurora_nwnmdlcomp::{
    CompileOptions, DecompileOptions, compile_bytes, convert_bytes, decompile_bytes, validate_bytes,
};
use clap::{Args, Parser, Subcommand};
use glob::{MatchOptions, Pattern, glob};
use nwnrs_types::{
    key::read_key_table_from_file,
    mdl::{MODEL_RES_TYPE, ModelEncoding},
    resman::{CachePolicy, ResContainer},
};
use rayon::prelude::*;
use tempfile::NamedTempFile;

#[derive(Debug, Parser)]
#[command(
    name = "nwnmdlcomp",
    version,
    about = "Modern Rust compiler/decompiler for Neverwinter Nights: Enhanced Edition MDL files",
    long_about = None,
    arg_required_else_help = true
)]
struct Cli {
    /// Worker threads used for multi-file operations (defaults to available CPUs).
    #[arg(long, global = true, value_name = "COUNT")]
    jobs: Option<usize>,

    /// Suppress successful per-file status messages.
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Compile ASCII MDL files into NWN:EE binary MDL files.
    Compile(CompileCommand),
    /// Decompile binary MDL files into canonical NWN:EE ASCII.
    Decompile(DecompileCommand),
    /// Detect each input encoding and convert it to the opposite encoding.
    Convert(ConvertCommand),
    /// Parse and deeply validate ASCII or binary MDL files without writing output.
    Validate(ValidateCommand),
    /// Extract MDL resources directly from an NWN KEY/BIF installation.
    Extract(ExtractCommand),
}

#[derive(Debug, Args)]
struct CommonTransform {
    /// Input paths. Shell-style wildcard arguments are expanded cross-platform.
    #[arg(required = true, value_name = "INPUT")]
    inputs: Vec<PathBuf>,

    /// Exact output path; only valid with one resolved input.
    #[arg(short, long, conflicts_with = "output_dir", value_name = "FILE")]
    output: Option<PathBuf>,

    /// Directory for generated files.
    #[arg(short = 'D', long, value_name = "DIR")]
    output_dir: Option<PathBuf>,

    /// Replace existing output files.
    #[arg(short, long)]
    force: bool,
}

#[derive(Debug, Args)]
struct CompileCommand {
    #[command(flatten)]
    common: CommonTransform,

    /// Reject legacy shorthand such as a nameless `donemodel` line.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct DecompileCommand {
    #[command(flatten)]
    common: CommonTransform,

    /// Embed the original binary in comments for byte-exact restoration.
    #[arg(long)]
    preserve_compiled_source: bool,
}

#[derive(Debug, Args)]
struct ConvertCommand {
    #[command(flatten)]
    common: CommonTransform,

    /// Embed the original binary when converting compiled MDL to ASCII.
    #[arg(long)]
    preserve_compiled_source: bool,
}

#[derive(Debug, Args)]
struct ValidateCommand {
    /// Input paths. Shell-style wildcard arguments are expanded cross-platform.
    #[arg(required = true, value_name = "INPUT")]
    inputs: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct ExtractCommand {
    /// Path to an NWN or NWN:EE KEY file; referenced BIFs are resolved beside it.
    #[arg(long, value_name = "FILE")]
    key: PathBuf,

    /// Case-insensitive model pattern, with or without the `.mdl` extension.
    #[arg(default_value = "*", value_name = "PATTERN")]
    pattern: String,

    /// Output directory for extracted models.
    #[arg(short = 'D', long, value_name = "DIR")]
    output_dir: PathBuf,

    /// Decompile each extracted binary model to ASCII.
    #[arg(short, long)]
    decompile: bool,

    /// Replace existing output files.
    #[arg(short, long)]
    force: bool,

    /// Embed original binary bytes in decompiled output.
    #[arg(long, requires = "decompile")]
    preserve_compiled_source: bool,
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    Compile { strict: bool },
    Decompile { preserve: bool },
    Convert { preserve: bool },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = configure_threads(cli.jobs).and_then(|()| run(&cli));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn configure_threads(jobs: Option<usize>) -> Result<()> {
    let Some(jobs) = jobs else {
        return Ok(());
    };
    if jobs == 0 {
        bail!("--jobs must be at least 1");
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build_global()
        .context("failed to configure worker pool")
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Compile(command) => run_transform(
            &command.common,
            Operation::Compile {
                strict: command.strict,
            },
            cli.quiet,
        ),
        Command::Decompile(command) => run_transform(
            &command.common,
            Operation::Decompile {
                preserve: command.preserve_compiled_source,
            },
            cli.quiet,
        ),
        Command::Convert(command) => run_transform(
            &command.common,
            Operation::Convert {
                preserve: command.preserve_compiled_source,
            },
            cli.quiet,
        ),
        Command::Validate(command) => run_validate(command, cli.quiet),
        Command::Extract(command) => run_extract(command, cli.quiet),
    }
}

fn run_transform(common: &CommonTransform, operation: Operation, quiet: bool) -> Result<()> {
    let inputs = expand_inputs(&common.inputs)?;
    if common.output.is_some() && inputs.len() != 1 {
        bail!(
            "--output requires exactly one resolved input, found {}",
            inputs.len()
        );
    }
    if let Some(directory) = &common.output_dir {
        fs::create_dir_all(directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
    }

    let results: Vec<Result<(PathBuf, PathBuf)>> = inputs
        .par_iter()
        .map(|input| {
            let source =
                fs::read(input).with_context(|| format!("failed to read {}", input.display()))?;
            let output = output_path(input, common, operation, &source)?;
            let transformed = match operation {
                Operation::Compile { strict } => compile_bytes(
                    &source,
                    CompileOptions {
                        legacy_compatibility: !strict,
                    },
                ),
                Operation::Decompile { preserve } => decompile_bytes(
                    &source,
                    DecompileOptions {
                        preserve_compiled_source: preserve,
                    },
                ),
                Operation::Convert { preserve } => convert_bytes(&source, preserve),
            }
            .with_context(|| format!("while processing {}", input.display()))?;
            atomic_write(&output, &transformed, common.force)?;
            Ok((input.clone(), output))
        })
        .collect();

    report_results(results, quiet)
}

fn run_validate(command: &ValidateCommand, quiet: bool) -> Result<()> {
    let inputs = expand_inputs(&command.inputs)?;
    let results: Vec<Result<String>> = inputs
        .par_iter()
        .map(|input| {
            let source =
                fs::read(input).with_context(|| format!("failed to read {}", input.display()))?;
            let report = validate_bytes(&source)
                .with_context(|| format!("validation failed for {}", input.display()))?;
            let encoding = match report.encoding {
                ModelEncoding::Ascii => "ASCII",
                ModelEncoding::Compiled => "binary",
            };
            Ok(format!(
                "{}: OK ({encoding}, model {}, {} nodes, {} animations)",
                input.display(),
                report.model_name,
                report.nodes,
                report.animations
            ))
        })
        .collect();

    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok(message) if !quiet => println!("{message}"),
            Ok(_) => {}
            Err(error) => failures.push(error),
        }
    }
    finish_failures(&failures)
}

fn run_extract(command: &ExtractCommand, quiet: bool) -> Result<()> {
    fs::create_dir_all(&command.output_dir)
        .with_context(|| format!("failed to create {}", command.output_dir.display()))?;
    let key = read_key_table_from_file(&command.key)
        .with_context(|| format!("failed to open {}", command.key.display()))?;
    let pattern_text = if command.pattern.to_ascii_lowercase().ends_with(".mdl") {
        command.pattern.clone()
    } else {
        format!("{}.mdl", command.pattern)
    };
    let pattern = Pattern::new(&pattern_text)
        .with_context(|| format!("invalid extraction pattern {pattern_text:?}"))?;
    let match_options = MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    let resources: Vec<_> = key
        .contents()
        .into_iter()
        .filter(|resource| resource.res_type() == MODEL_RES_TYPE)
        .filter(|resource| {
            resource
                .resolve()
                .is_some_and(|resolved| pattern.matches_with(&resolved.to_file(), match_options))
        })
        .collect();
    if resources.is_empty() {
        bail!("no MDL resources matched {:?}", command.pattern);
    }

    let mut failures = Vec::new();
    for resource in resources {
        let result = (|| -> Result<PathBuf> {
            let resolved = resource
                .resolve()
                .context("MDL resource type has no registered extension")?;
            let model = key
                .demand(&resource)
                .with_context(|| format!("failed to load {resolved}"))?
                .read_all(CachePolicy::Bypass)
                .with_context(|| format!("failed to read {resolved}"))?;
            if model.is_empty() {
                bail!(
                    "{resolved} has an empty payload; dedicated-server data packages commonly omit client model data"
                );
            }
            let (filename, output) = if command.decompile {
                (
                    format!("{}.ascii", resolved.to_file()),
                    match nwnrs_types::mdl::detect_model_encoding(&model) {
                        ModelEncoding::Ascii => model,
                        ModelEncoding::Compiled => decompile_bytes(
                            &model,
                            DecompileOptions {
                                preserve_compiled_source: command.preserve_compiled_source,
                            },
                        )?,
                    },
                )
            } else {
                (resolved.to_file(), model)
            };
            let path = command.output_dir.join(filename);
            atomic_write(&path, &output, command.force)?;
            Ok(path)
        })();

        match result {
            Ok(path) if !quiet => println!("extracted {}", path.display()),
            Ok(_) => {}
            Err(error) => failures.push(error),
        }
    }
    finish_failures(&failures)
}

fn output_path(
    input: &Path,
    common: &CommonTransform,
    operation: Operation,
    source: &[u8],
) -> Result<PathBuf> {
    if let Some(output) = &common.output {
        return Ok(output.clone());
    }
    let filename = input
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("input path has no UTF-8 filename: {}", input.display()))?;
    let output_name = match operation {
        Operation::Compile { .. } => compiled_name(filename),
        Operation::Decompile { .. } => format!("{filename}.ascii"),
        Operation::Convert { .. } => match nwnrs_types::mdl::detect_model_encoding(source) {
            ModelEncoding::Ascii => compiled_name(filename),
            ModelEncoding::Compiled => format!("{filename}.ascii"),
        },
    };
    let directory = common
        .output_dir
        .as_deref()
        .unwrap_or_else(|| input.parent().unwrap_or_else(|| Path::new(".")));
    Ok(directory.join(output_name))
}

fn compiled_name(filename: &str) -> String {
    if let Some(stem) = filename.strip_suffix(".ascii") {
        stem.to_owned()
    } else if let Some(stem) = filename.strip_suffix(".mdl.ascii") {
        format!("{stem}.mdl")
    } else if let Some(stem) = filename.strip_suffix(".ascii.mdl") {
        format!("{stem}.mdl")
    } else if filename.to_ascii_lowercase().ends_with(".mdl") {
        let stem = &filename[..filename.len() - 4];
        format!("{stem}.compiled.mdl")
    } else {
        format!("{filename}.mdl")
    }
}

fn expand_inputs(arguments: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut inputs = Vec::new();
    for argument in arguments {
        let text = argument.to_string_lossy();
        if argument.is_file() {
            // Existing paths are always literal. This matters for legitimate
            // model filenames containing glob metacharacters such as `[`.
            inputs.push(argument.clone());
        } else if text.contains(['*', '?', '[']) {
            let mut matched = false;
            for entry in glob(&text).with_context(|| format!("invalid input pattern {text:?}"))? {
                let path = entry.with_context(|| format!("failed to expand pattern {text:?}"))?;
                if path.is_file() {
                    inputs.push(path);
                    matched = true;
                }
            }
            if !matched {
                bail!("input pattern {text:?} matched no files");
            }
        } else {
            bail!("input file does not exist: {}", argument.display());
        }
    }
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

fn atomic_write(path: &Path, data: &[u8], force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!("output exists (use --force): {}", path.display());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    temporary
        .write_all(data)
        .with_context(|| format!("failed to write temporary output for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("failed to sync temporary output for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| anyhow!("failed to persist {}: {}", path.display(), error.error))?;
    Ok(())
}

fn report_results(results: Vec<Result<(PathBuf, PathBuf)>>, quiet: bool) -> Result<()> {
    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok((input, output)) if !quiet => {
                println!("{} -> {}", input.display(), output.display());
            }
            Ok(_) => {}
            Err(error) => failures.push(error),
        }
    }
    finish_failures(&failures)
}

fn finish_failures(failures: &[anyhow::Error]) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    for error in failures {
        eprintln!("error: {error:#}");
    }
    bail!("{} file(s) failed", failures.len())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{compiled_name, expand_inputs};

    #[test]
    fn derives_non_destructive_output_names() {
        assert_eq!(compiled_name("foo.mdl.ascii"), "foo.mdl");
        assert_eq!(compiled_name("foo.ascii.mdl"), "foo.mdl");
        assert_eq!(compiled_name("foo.mdl"), "foo.compiled.mdl");
        assert_eq!(compiled_name("foo"), "foo.mdl");
    }

    #[test]
    fn existing_paths_with_glob_characters_are_literal() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("mesh[old].mdl");
        std::fs::write(&path, b"model")?;
        assert_eq!(expand_inputs(&[PathBuf::from(&path)])?, vec![path]);
        Ok(())
    }
}
