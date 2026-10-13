use clap::Parser;
use ldu::{
    backend::Backend,
    config::{self, Config, MAX_DEBOUNCE_MS, Settings},
    model::Model,
    scan::{self, ScanOptions},
    theme::ThemeColor,
    types::{ROOT, SizeMode},
};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io,
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(
    version,
    disable_version_flag = true,
    about = "Live disk usage with effort-first scanning; ncdu-style navigation"
)]
struct Args {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(short = 't', short_alias = 'j', long)]
    threads: Option<usize>,
    #[arg(long, value_enum)]
    backend: Option<Backend>,
    #[arg(long)]
    buffer_size: Option<usize>,
    #[arg(long)]
    batch_size: Option<usize>,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    apparent_size: Option<bool>,
    #[arg(
        long,
        value_name = "COLOR",
        help = "Accent: ANSI name, 0–255, or #RRGGBB"
    )]
    color: Option<ThemeColor>,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", help = "Enable mouse input (or --mouse=false)")]
    mouse: Option<bool>,
    #[arg(long, conflicts_with = "mouse", help = "Disable mouse input")]
    no_mouse: bool,
    #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u64).range(0..=MAX_DEBOUNCE_MS), help = "Recent mouse selection window; 0 disables the guard (default: 250)")]
    mouse_debounce_ms: Option<u64>,
    #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u64).range(0..=MAX_DEBOUNCE_MS), help = "Confirmation delay after recent mouse selection (default: 1250)")]
    mouse_debounce_delay_ms: Option<u64>,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", help = "Show the mouse-selection warning (default: true)")]
    mouse_debounce_warning: Option<bool>,
    #[arg(
        short = 'o',
        long,
        value_name = "FILE",
        help = "Write an ncdu-compatible JSON tree; use - for stdout"
    )]
    output: Option<PathBuf>,
    #[arg(
        short = '0',
        requires = "output",
        help = "Export without a scan interface"
    )]
    _quiet: bool,
    #[arg(short = 'v', short_alias = 'V', long = "version", action = clap::ArgAction::Version)]
    _version: Option<bool>,
}

fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().min(32))
}

impl Args {
    fn resolve(&self, config: Config) -> io::Result<(ScanOptions, SizeMode, Settings)> {
        let options = ScanOptions {
            threads: self
                .threads
                .or(config.threads)
                .unwrap_or_else(default_threads),
            batch_size: self.batch_size.or(config.batch_size).unwrap_or(128),
            buffer_size: self.buffer_size.or(config.buffer_size).unwrap_or(262_144),
            backend: self.backend.or(config.backend).unwrap_or(Backend::Auto),
        };
        if !(1..=128).contains(&options.threads)
            || !(1..=4096).contains(&options.batch_size)
            || !(4096..=4_194_304).contains(&options.buffer_size)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "threads must be 1–128, batch-size 1–4096, and buffer-size 4096–4194304",
            ));
        }
        let settings = Settings {
            color: self.color.unwrap_or(config.ui.color),
            mouse: !self.no_mouse && self.mouse.unwrap_or(config.ui.mouse),
            mouse_debounce_ms: self
                .mouse_debounce_ms
                .unwrap_or(config.ui.mouse_debounce_ms),
            mouse_debounce_delay_ms: self
                .mouse_debounce_delay_ms
                .unwrap_or(config.ui.mouse_debounce_delay_ms),
            mouse_debounce_warning: self
                .mouse_debounce_warning
                .unwrap_or(config.ui.mouse_debounce_warning),
        };
        let mode = if self.apparent_size.or(config.apparent_size).unwrap_or(false) {
            SizeMode::Apparent
        } else {
            SizeMode::Allocated
        };
        Ok((options, mode, settings))
    }
}

fn run(args: Args) -> io::Result<bool> {
    // clap handles help/version before configuration is opened.
    let (options, mode, settings) = args.resolve(config::load()?)?;
    let root = if args.path.is_absolute() {
        args.path
    } else {
        std::env::current_dir()?.join(args.path)
    };
    let Some(output) = args.output else {
        ldu::tui::run(root, options, mode, settings)?;
        return Ok(false);
    };
    // Only file output needs this identity snapshot. The file is opened after
    // scanning so a newly created export does not scan its own growing bytes.
    let root_metadata = (output != Path::new("-"))
        .then(|| fs::metadata(&root).ok())
        .flatten();
    let mut model = Model::new(root.clone());
    let handle = scan::spawn(root.clone(), options);
    for event in &handle.receiver {
        model.apply(event);
    }
    let node = model.node(ROOT);
    let failed = node.error_count != 0 || model.cancelled || !model.finished;
    if output == Path::new("-") {
        ldu::export::write_ncdu(&model, &root, io::stdout().lock())?;
    } else {
        let file = create_export(&output, &root, root_metadata.as_ref())?;
        ldu::export::write_ncdu(&model, &root, file)?;
    }
    Ok(failed)
}

// Resolve existing aliases and also compare destinations whose leaf is absent.
fn destination(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize().or_else(|_| {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        match (absolute.parent(), absolute.file_name()) {
            (Some(parent), Some(name)) => Ok(parent.canonicalize()?.join(name)),
            _ => Ok(absolute),
        }
    })
}

#[cfg(unix)]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_: &Metadata, _: &Metadata) -> bool {
    false
}

fn create_export(output: &Path, root: &Path, initial: Option<&Metadata>) -> io::Result<File> {
    let refused = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "export file must not replace the scan root",
        )
    };
    if let (Ok(output), Ok(root)) = (destination(output), destination(root))
        && output == root
    {
        return Err(refused());
    }
    // Check the opened file before truncating it: pathname-only checks miss
    // hardlinks, and following an output symlink must not truncate the root.
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(output)?;
    let metadata = file.metadata()?;
    if initial.is_some_and(|root| same_file(&metadata, root))
        || fs::metadata(root).is_ok_and(|root| same_file(&metadata, &root))
    {
        return Err(refused());
    }
    if metadata.is_file() {
        file.set_len(0)?;
    }
    Ok(file)
}

fn main() {
    match run(Args::parse()) {
        Ok(false) => (),
        Ok(true) => std::process::exit(2),
        Err(error) => {
            eprintln!("ldu: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[cfg(unix)]
    #[test]
    fn export_accepts_a_writable_device_without_truncating_it() {
        let root = Path::new("/");
        let file = create_export(Path::new("/dev/null"), root, None).unwrap();
        ldu::export::write_ncdu(&Model::new(root.into()), root, file).unwrap();
    }

    #[test]
    fn ncdu_export_flags_and_thread_aliases_parse() {
        for threads in ["-t", "-j", "--threads"] {
            let args = Args::try_parse_from([
                "ldu",
                "-0",
                "-o",
                "-",
                threads,
                "3",
                "--apparent-size",
                "/fixture",
            ])
            .unwrap();
            assert_eq!(args.output.as_deref(), Some(Path::new("-")));
            assert_eq!(args.threads, Some(3));
            assert_eq!(args.path, Path::new("/fixture"));
            assert_eq!(args.apparent_size, Some(true));
            assert!(args._quiet);
        }
        let args = Args::try_parse_from(["ldu", "--output", "tree.json"]).unwrap();
        assert_eq!(args.output.as_deref(), Some(Path::new("tree.json")));
        assert!(!args._quiet);
        assert!(Args::try_parse_from(["ldu"]).unwrap().output.is_none());
    }

    #[test]
    fn ncdu_version_aliases_and_invalid_export_options() {
        for version in ["-v", "-V", "--version"] {
            let error = Args::try_parse_from(["ldu", version]).err().unwrap();
            assert_eq!(error.kind(), ErrorKind::DisplayVersion);
            assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
        }
        for args in [
            vec!["ldu", "--json"],
            vec!["ldu", "-0"],
            vec!["ldu", "-o"],
            vec!["ldu", "-o", "one", "-o", "two"],
            vec!["ldu", "-o", "-", "-t", "many"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn cli_preserves_non_utf8_scan_and_export_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let scan = OsString::from_vec(b"scan-\xff".to_vec());
        let output = OsString::from_vec(b"export-\xfe".to_vec());
        let args = Args::try_parse_from([
            OsString::from("ldu"),
            scan.clone(),
            OsString::from("-o"),
            output.clone(),
        ])
        .unwrap();
        assert_eq!(args.path, PathBuf::from(scan));
        assert_eq!(args.output, Some(PathBuf::from(output)));
    }

    #[test]
    fn cli_help_and_errors_retain_usage_and_argument_context() {
        let help = Args::try_parse_from(["ldu", "--help"]).err().unwrap();
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        let text = help.to_string();
        for expected in [
            "Usage:",
            "--output",
            "--threads",
            "--color",
            "--no-mouse",
            "--version",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        let error = Args::try_parse_from(["ldu", "--backend", "bogus"])
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        let text = error.to_string();
        for expected in ["--backend", "bogus", "auto", "std"] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        let error = Args::try_parse_from(["ldu", "--threds"]).err().unwrap();
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        let text = error.to_string();
        assert!(
            text.contains("--threds") && text.contains("Usage:"),
            "{text}"
        );
    }

    #[test]
    fn cli_overrides_config_including_explicit_false_values() {
        let config = || Config {
            ui: Settings {
                color: "red".parse().unwrap(),
                mouse: false,
                mouse_debounce_ms: 123,
                mouse_debounce_delay_ms: 456,
                mouse_debounce_warning: false,
            },
            threads: Some(7),
            backend: Some(Backend::Native),
            buffer_size: Some(8192),
            batch_size: Some(64),
            apparent_size: Some(true),
        };
        let (scan, mode, ui) = Args::try_parse_from(["ldu"])
            .unwrap()
            .resolve(config())
            .unwrap();
        assert_eq!(scan.threads, 7);
        assert!(matches!(scan.backend, Backend::Native));
        assert_eq!(scan.buffer_size, 8192);
        assert_eq!(scan.batch_size, 64);
        assert!(matches!(mode, SizeMode::Apparent));
        assert_eq!(ui, config().ui);

        let args = Args::try_parse_from([
            "ldu",
            "--color",
            "#abc123",
            "--mouse",
            "--mouse-debounce-ms",
            "0",
            "--mouse-debounce-delay-ms",
            "60000",
            "--mouse-debounce-warning",
            "--threads",
            "3",
            "--backend",
            "std",
            "--buffer-size",
            "4096",
            "--batch-size",
            "2",
            "--apparent-size=false",
        ])
        .unwrap();
        let (scan, mode, ui) = args.resolve(config()).unwrap();
        assert_eq!(scan.threads, 3);
        assert!(matches!(scan.backend, Backend::Std));
        assert_eq!(scan.buffer_size, 4096);
        assert_eq!(scan.batch_size, 2);
        assert!(matches!(mode, SizeMode::Allocated));
        assert_eq!(ui.color, "#abc123".parse().unwrap());
        assert!(ui.mouse && ui.mouse_debounce_warning);
        assert_eq!(ui.mouse_debounce_ms, 0);
        assert_eq!(ui.mouse_debounce_delay_ms, 60000);

        for flag in ["--no-mouse", "--mouse=false"] {
            let (_, _, ui) = Args::try_parse_from(["ldu", flag, "--mouse-debounce-warning=false"])
                .unwrap()
                .resolve(Config::default())
                .unwrap();
            assert!(!ui.mouse && !ui.mouse_debounce_warning);
        }
    }

    #[test]
    fn cli_settings_reject_invalid_values_and_mouse_conflicts() {
        for args in [
            vec!["ldu", "--mouse", "--no-mouse"],
            vec!["ldu", "--mouse=maybe"],
            vec!["ldu", "--mouse-debounce-ms", "60001"],
            vec!["ldu", "--mouse-debounce-delay-ms", "18446744073709551615"],
            vec!["ldu", "--mouse-debounce-warning=no"],
            vec!["ldu", "--color", "#12345"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
        for (flag, value) in [
            ("--threads", "0"),
            ("--batch-size", "4097"),
            ("--buffer-size", "4095"),
        ] {
            let args = Args::try_parse_from(["ldu", flag, value]).unwrap();
            assert!(args.resolve(Config::default()).is_err());
        }
        for args in [["ldu", "--help"], ["ldu", "--version"]] {
            let error = Args::try_parse_from(args).err().unwrap();
            assert!(matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ));
        }
    }
}
