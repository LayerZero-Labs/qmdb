use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the directory where trickfs will be mounted
    #[arg(short, long, default_value = "/tmp/trick")]
    mountpoint: String,
}

fn waitline() {
    log::info!("press return to stop...");
    let _ = std::io::stdin().read_line(&mut String::new());
}

fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    let args = Args::parse();

    let handle = trickfs::spawn_trick(args.mountpoint, 0).unwrap();
    waitline();
    drop(handle);

    Ok(())
}
