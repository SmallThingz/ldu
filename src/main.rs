use clap::Parser;
use ldu::{
    backend::Backend,
    model::Model,
    scan::{self, ScanOptions},
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
    #[arg(short = 't', short_alias = 'j', long, default_value_t = default_threads(), value_parser = clap::value_parser!(usize))]
    threads: usize,
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    backend: Backend,
    #[arg(long, default_value_t = 262144)]
    buffer_size: usize,
    #[arg(long, default_value_t = 128)]
    batch_size: usize,
    #[arg(long)]
    apparent_size: bool,
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

fn run(args: Args) -> io::Result<bool> {
    if !(1..=128).contains(&args.threads)
        || !(1..=4096).contains(&args.batch_size)
        || !(4096..=4194304).contains(&args.buffer_size)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "threads must be 1–128, batch-size 1–4096, and buffer-size 4096–4194304",
        ));
    }
    let options = ScanOptions {
        threads: args.threads,
        batch_size: args.batch_size,
        buffer_size: args.buffer_size,
        backend: args.backend,
    };
    let mode = if args.apparent_size {
        SizeMode::Apparent
    } else {
        SizeMode::Allocated
    };
    let root = if args.path.is_absolute() {
        args.path
    } else {
        std::env::current_dir()?.join(args.path)
    };
    let Some(output) = args.output else {
        ldu::tui::run(root, options, mode)?;
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
            assert_eq!(args.threads, 3);
            assert_eq!(args.path, Path::new("/fixture"));
            assert!(args.apparent_size);
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

}
