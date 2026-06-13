use anyhow::Context;
use clap::Parser;
use eth_replay_kv::{
    new_s3_client, parse_storage_path, read_bytes, s3_key, upload_file_to_s3, StoragePath,
};
use flate2::write::GzEncoder;
use flate2::Compression;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(
    name = "kv-compress",
    about = "Compresses KV operation files into archives",
    long_about = "Reads `{height}_base_kv.bin` and `{height}_operation_kv.bin` files and compresses them into archives with configurable blocks per archive. Input and output may be local directories or S3 URIs (s3://bucket/prefix)."
)]
struct Args {
    /// First block height (inclusive).
    start_height: u64,

    /// Last block height (inclusive).
    end_height: u64,

    /// Directory containing `{height}_base_kv.bin` and `{height}_operation_kv.bin` files.
    input: String,

    /// Output directory for archive files.
    output: String,

    /// Number of blocks to include in each archive.
    #[arg(long, default_value = "10000")]
    blocks_per_zip: u64,

    /// Archive format: "tar.gz" (default) or "zip".
    #[arg(long, default_value = "tar.gz")]
    format: String,

    /// Compression level (1-9, default 9 for best compression).
    #[arg(long, default_value = "9")]
    level: u32,

    /// Maximum number of concurrent S3 requests per chunk.
    #[arg(long, default_value = "256")]
    concurrency: usize,

    /// Number of archives to process in parallel (for CPU parallelism).
    /// Defaults to the number of CPU cores.
    #[arg(long)]
    parallel_zips: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
enum ArchiveFormat {
    TarGz { level: u32 },
    Zip { level: u32 },
}

fn parse_archive_format(format_str: &str, level: u32) -> anyhow::Result<ArchiveFormat> {
    if !(1..=9).contains(&level) {
        anyhow::bail!("compression level must be between 1 and 9");
    }

    match format_str.to_lowercase().as_str() {
        "tar.gz" | "tgz" | "gz" => Ok(ArchiveFormat::TarGz { level }),
        "zip" => Ok(ArchiveFormat::Zip { level }),
        other => anyhow::bail!(
            "Unknown format '{}'. Use 'tar.gz' (recommended) or 'zip'.",
            other
        ),
    }
}

/// Represents a file to be fetched
#[derive(Clone)]
struct FileToFetch {
    filename: String,
}

/// Result of fetching a file
struct FetchedFile {
    filename: String,
    data: Option<Vec<u8>>,
}

/// A chunk of blocks to be compressed into one archive
#[derive(Clone)]
struct ChunkSpec {
    chunk_start: u64,
    chunk_end: u64,
}

/// Result of processing a chunk
struct ChunkResult {
    files_count: u64,
    raw_bytes: u64,
    compressed_bytes: u64,
    skipped: bool,
}

/// Fetch files in parallel with bounded concurrency
async fn fetch_files_parallel(
    files: Vec<FileToFetch>,
    source: &StoragePath,
    s3_client: Option<Arc<aws_sdk_s3::Client>>,
    concurrency: usize,
) -> anyhow::Result<Vec<FetchedFile>> {
    let results: Vec<anyhow::Result<FetchedFile>> = stream::iter(files)
        .map(|file| {
            let source = source.clone();
            let client = s3_client.clone();
            async move {
                let data =
                    read_bytes(&source, client.as_deref(), &file.filename).await?;
                Ok(FetchedFile {
                    filename: file.filename,
                    data,
                })
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    results.into_iter().collect()
}

/// Write a tar.gz archive
fn write_tar_gz(
    archive_path: &PathBuf,
    files: Vec<FetchedFile>,
    level: u32,
) -> anyhow::Result<(u64, u64, u64)> {
    let file = File::create(archive_path)
        .with_context(|| format!("Failed to create archive {:?}", archive_path))?;

    // Create gzip encoder with specified level
    let encoder = GzEncoder::new(file, Compression::new(level));

    // Create tar archive
    let mut tar = tar::Builder::new(encoder);

    let mut files_count = 0u64;
    let mut raw_bytes = 0u64;

    for fetched in files {
        if let Some(data) = fetched.data {
            let mut header = tar::Header::new_gnu();
            header.set_path(&fetched.filename)?;
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0); // Reproducible builds
            header.set_cksum();

            tar.append(&header, data.as_slice())?;
            files_count += 1;
            raw_bytes += data.len() as u64;
        }
    }

    tar.finish()?;
    drop(tar);

    let compressed_bytes = fs::metadata(archive_path)?.len();
    Ok((files_count, raw_bytes, compressed_bytes))
}

/// Write a zip archive
fn write_zip(
    archive_path: &PathBuf,
    files: Vec<FetchedFile>,
    level: u32,
) -> anyhow::Result<(u64, u64, u64)> {
    use zip::write::FileOptions;
    use zip::CompressionMethod;
    use zip::ZipWriter;

    let file = File::create(archive_path)
        .with_context(|| format!("Failed to create archive {:?}", archive_path))?;
    let mut zip = ZipWriter::new(file);

    let options: FileOptions<'_, ()> = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(level as i64));

    let mut files_count = 0u64;
    let mut raw_bytes = 0u64;

    for fetched in files {
        if let Some(data) = fetched.data {
            zip.start_file(&fetched.filename, options.clone())?;
            zip.write_all(&data)?;
            files_count += 1;
            raw_bytes += data.len() as u64;
        }
    }

    zip.finish()?;

    let compressed_bytes = fs::metadata(archive_path)?.len();
    Ok((files_count, raw_bytes, compressed_bytes))
}

/// Process a single chunk: fetch files, compress, write archive (and upload to S3 if destination is S3).
async fn process_chunk(
    chunk: ChunkSpec,
    source: StoragePath,
    s3_client: Option<Arc<aws_sdk_s3::Client>>,
    output_dir: PathBuf,
    destination: StoragePath,
    format: ArchiveFormat,
    concurrency: usize,
    progress_bar: ProgressBar,
) -> anyhow::Result<ChunkResult> {
    let num_blocks = chunk.chunk_end - chunk.chunk_start + 1;
    progress_bar.set_message(format!("Fetching {} blocks", num_blocks));

    let mut files_to_fetch = Vec::new();
    for height in chunk.chunk_start..=chunk.chunk_end {
        files_to_fetch.push(FileToFetch {
            filename: format!("{}_base_kv.bin", height),
        });
        files_to_fetch.push(FileToFetch {
            filename: format!("{}_operation_kv.bin", height),
        });
    }

    // Fetch all files in parallel
    let fetched_files = fetch_files_parallel(files_to_fetch, &source, s3_client.clone(), concurrency).await?;

    progress_bar.set_message("Compressing");

    // Determine archive filename
    let archive_filename = match format {
        ArchiveFormat::TarGz { .. } => {
            format!("kv_{}_{}.tar.gz", chunk.chunk_start, chunk.chunk_end)
        }
        ArchiveFormat::Zip { .. } => {
            format!("kv_{}_{}.zip", chunk.chunk_start, chunk.chunk_end)
        }
    };
    let archive_path = output_dir.join(&archive_filename);
    let archive_path_blocking = archive_path.clone();

    // Create the archive in a blocking task
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<ChunkResult> {
        // Sort files for consistent ordering
        let mut fetched_files = fetched_files;
        fetched_files.sort_by(|a, b| a.filename.cmp(&b.filename));

        let (files_count, raw_bytes, compressed_bytes) = match format {
            ArchiveFormat::TarGz { level } => write_tar_gz(&archive_path_blocking, fetched_files, level)?,
            ArchiveFormat::Zip { level } => write_zip(&archive_path_blocking, fetched_files, level)?,
        };

        if files_count > 0 {
            Ok(ChunkResult {
                files_count,
                raw_bytes,
                compressed_bytes,
                skipped: false,
            })
        } else {
            // Remove empty archive
            let _ = fs::remove_file(&archive_path_blocking);
            Ok(ChunkResult {
                files_count: 0,
                raw_bytes: 0,
                compressed_bytes: 0,
                skipped: true,
            })
        }
    })
    .await??;

    // When destination is S3, upload the archive and remove the local temp file.
    if let StoragePath::S3 { bucket, prefix } = &destination {
        if let Some(ref client) = s3_client {
            if archive_path.exists() {
                let key = s3_key(prefix, &archive_filename);
                progress_bar.set_message("Uploading to S3");
                upload_file_to_s3(client, bucket, &key, &archive_path).await?;
                fs::remove_file(&archive_path)
                    .with_context(|| format!("Failed to remove temp file {:?}", archive_path))?;
            }
        }
    }

    progress_bar.finish_and_clear();
    Ok(result)
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.start_height > args.end_height {
        anyhow::bail!("start_height must be <= end_height");
    }

    if args.blocks_per_zip == 0 {
        anyhow::bail!("blocks_per_zip must be > 0");
    }

    // Default parallel_zips to number of CPU cores
    let num_cpus = num_cpus::get();
    let parallel_zips = args.parallel_zips.unwrap_or(num_cpus);

    let format = parse_archive_format(&args.format, args.level)?;
    let source = parse_storage_path(&args.input);
    let destination = parse_storage_path(&args.output);

    // Output: local dir or temp dir when destination is S3.
    let output_dir: PathBuf = match &destination {
        StoragePath::LocalDir(p) => {
            fs::create_dir_all(p)
                .with_context(|| format!("Failed to create output directory {:?}", p))?;
            p.clone()
        }
        StoragePath::S3 { .. } => {
            let temp = std::env::temp_dir().join(format!("kv_compress_{}", std::process::id()));
            fs::create_dir_all(&temp)
                .with_context(|| format!("Failed to create temp directory {:?}", temp))?;
            temp
        }
    };

    // S3 client when input or output is S3 (one client for the run, same as other binaries).
    let s3_client: Option<Arc<aws_sdk_s3::Client>> =
        if source.is_s3() || destination.is_s3() {
            Some(Arc::new(new_s3_client().await))
        } else {
            None
        };

    let total_blocks = args.end_height - args.start_height + 1;

    // Build list of all chunks
    let mut chunks = Vec::new();
    let mut current_height = args.start_height;
    while current_height <= args.end_height {
        let chunk_start = current_height;
        let chunk_end = std::cmp::min(current_height + args.blocks_per_zip - 1, args.end_height);
        chunks.push(ChunkSpec {
            chunk_start,
            chunk_end,
        });
        current_height = chunk_end + 1;
    }

    let total_chunks = chunks.len();

    // Print configuration
    let (format_name, level) = match format {
        ArchiveFormat::TarGz { level } => ("TAR.GZ", level),
        ArchiveFormat::Zip { level } => ("ZIP", level),
    };

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                     KV Compress                              ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!(
        "║ Blocks: {:>12} - {:<12} ({:>10} total)    ║",
        args.start_height, args.end_height, total_blocks
    );
    println!(
        "║ Chunks: {:>10} archives ({} blocks each)             ║",
        total_chunks, args.blocks_per_zip
    );
    println!(
        "║ Format: {:<10} (level {})                                  ║",
        format_name, level
    );
    println!(
        "║ Parallelism: {} archives ({} CPUs), {} S3 reqs/chunk        ║",
        parallel_zips, num_cpus, args.concurrency
    );
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    let start_time = Instant::now();

    // Set up progress tracking
    let multi_progress = MultiProgress::new();

    // Overall progress bar
    let overall_style = ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) | {msg}")
        .unwrap()
        .progress_chars("█▓▒░  ");

    let overall_pb = multi_progress.add(ProgressBar::new(total_chunks as u64));
    overall_pb.set_style(overall_style);
    overall_pb.set_message("Starting...");

    // Style for individual chunk progress bars
    let chunk_style = ProgressStyle::default_spinner()
        .template("  {spinner:.yellow} {prefix}: {msg}")
        .unwrap();

    // Counters for progress
    let total_files = Arc::new(AtomicU64::new(0));
    let total_raw_bytes = Arc::new(AtomicU64::new(0));
    let total_compressed_bytes = Arc::new(AtomicU64::new(0));
    let archive_count = Arc::new(AtomicU64::new(0));

    // Process chunks in parallel with bounded concurrency
    let results: Vec<anyhow::Result<ChunkResult>> = stream::iter(chunks)
        .map(|chunk| {
            let source = source.clone();
            let s3_client = s3_client.clone();
            let output_dir = output_dir.clone();
            let destination = destination.clone();
            let files_counter = total_files.clone();
            let raw_counter = total_raw_bytes.clone();
            let compressed_counter = total_compressed_bytes.clone();
            let archive_counter = archive_count.clone();
            let concurrency = args.concurrency;
            let overall_pb = overall_pb.clone();

            // Create a progress bar for this chunk
            let chunk_pb = multi_progress.insert_before(&overall_pb, ProgressBar::new_spinner());
            chunk_pb.set_style(chunk_style.clone());
            chunk_pb.set_prefix(format!("{}..{}", chunk.chunk_start, chunk.chunk_end));
            chunk_pb.enable_steady_tick(std::time::Duration::from_millis(100));

            async move {
                let result = process_chunk(
                    chunk.clone(),
                    source,
                    s3_client,
                    output_dir,
                    destination,
                    format,
                    concurrency,
                    chunk_pb,
                )
                .await?;

                // Update counters
                if !result.skipped {
                    files_counter.fetch_add(result.files_count, Ordering::Relaxed);
                    raw_counter.fetch_add(result.raw_bytes, Ordering::Relaxed);
                    compressed_counter.fetch_add(result.compressed_bytes, Ordering::Relaxed);
                    archive_counter.fetch_add(1, Ordering::Relaxed);
                }

                // Update overall progress
                overall_pb.inc(1);

                let raw = raw_counter.load(Ordering::Relaxed);
                let compressed = compressed_counter.load(Ordering::Relaxed);
                let ratio = if raw > 0 {
                    (compressed as f64 / raw as f64) * 100.0
                } else {
                    0.0
                };
                overall_pb.set_message(format!(
                    "{} -> {} ({:.1}%)",
                    format_bytes(raw),
                    format_bytes(compressed),
                    ratio
                ));

                Ok(result)
            }
        })
        .buffer_unordered(parallel_zips)
        .collect()
        .await;

    overall_pb.finish_with_message("Done!");

    // Check for errors
    for result in results {
        result?;
    }

    let elapsed = start_time.elapsed();
    let raw = total_raw_bytes.load(Ordering::Relaxed);
    let compressed = total_compressed_bytes.load(Ordering::Relaxed);
    let files = total_files.load(Ordering::Relaxed);
    let archives = archive_count.load(Ordering::Relaxed);

    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                        Summary                               ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!(
        "║ Time elapsed:        {:>40} ║",
        format!("{:.2?}", elapsed)
    );
    println!(
        "║ Archives created:    {:>40} ║",
        archives
    );
    println!(
        "║ Files compressed:    {:>40} ║",
        files
    );
    println!(
        "║ Raw size:            {:>40} ║",
        format_bytes(raw)
    );
    println!(
        "║ Compressed size:     {:>40} ║",
        format_bytes(compressed)
    );
    if raw > 0 {
        let ratio = (compressed as f64 / raw as f64) * 100.0;
        let savings = 100.0 - ratio;
        println!(
            "║ Compression ratio:   {:>40} ║",
            format!("{:.1}% (saved {:.1}%)", ratio, savings)
        );
        let throughput = raw as f64 / elapsed.as_secs_f64();
        println!(
            "║ Throughput:          {:>40} ║",
            format!("{}/s", format_bytes(throughput as u64))
        );
    }
    println!("╚══════════════════════════════════════════════════════════════╝");

    Ok(())
}
