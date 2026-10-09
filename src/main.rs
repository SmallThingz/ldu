use clap::Parser;
use ldu::{
    backend::Backend,
    model::Model,
    scan::{self, ScanOptions},
    types::{NodeKind, ROOT, SizeMode},
};
use std::{io, path::PathBuf, time::Instant};

#[derive(Parser)]
#[command(
    version,
    about = "Live disk usage with effort-first scanning; ncdu-style navigation"
)]
struct Args {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(short = 'j', long, default_value_t = default_threads(), value_parser = clap::value_parser!(usize))]
    threads: usize,
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    backend: Backend,
    #[arg(long, default_value_t = 262144)]
    buffer_size: usize,
    #[arg(long, default_value_t = 128)]
    batch_size: usize,
    #[arg(long)]
    apparent_size: bool,
    #[arg(long, help = "Scan without a terminal and print aggregate JSON")]
    json: bool,
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
    let root = if args.path.is_absolute() {
        args.path
    } else {
        std::env::current_dir()?.join(args.path)
    };
    let options = ScanOptions {
        threads: args.threads,
        batch_size: args.batch_size,
        buffer_size: args.buffer_size,
        backend: args.backend,
    };
    if !args.json {
        ldu::tui::run(
            root,
            options,
            if args.apparent_size {
                SizeMode::Apparent
            } else {
                SizeMode::Allocated
            },
        )?;
        return Ok(false);
    }
    let start = Instant::now();
    let mut model = Model::new(root.clone());
    let handle = scan::spawn(root.clone(), options);
    for event in &handle.receiver {
        model.apply(event);
    }
    let node = model.node(ROOT);
    let directories: Vec<_> = model
        .children(ROOT)
        .filter(|&id| model.node(id).kind == NodeKind::Directory)
        .map(|id| {
            let node = model.node(id);
            serde_json::json!({
                "name": node.name.to_string_lossy(),
                "path": model.path(id).to_string_lossy(),
                "apparent": node.sizes.apparent,
                "allocated": node.sizes.allocated,
                "complete": node.complete,
                "errors": node.error_count,
            })
        })
        .collect();
    let failed = node.error_count != 0 || model.cancelled || !model.finished;
    let output = serde_json::json!({
        "root": root.to_string_lossy(), "apparent": node.sizes.apparent, "allocated": node.sizes.allocated,
        "entries": node.items, "errors": node.error_count, "cancelled": model.cancelled,
        "seconds": start.elapsed().as_secs_f64(), "directories": directories,
    });
    serde_json::to_writer(io::stdout().lock(), &output)?;
    println!();
    Ok(failed)
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
