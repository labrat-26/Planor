//! `planor-rec`: record the desktop on one clock, then check Planor's claims against pixels.

use clap::{Parser, Subcommand};
use planor_rec::{analyze, capture, pipe, prune, record, review, ring, util};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "planor-rec", version, about = "Planor screen recorder and pixel analyzer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record all monitors (or one) until `stop`, Ctrl+C or Ctrl+Alt+Shift+R.
    Record {
        /// Recording directory (created if missing).
        #[arg(long)]
        out: PathBuf,
        /// Also record the microphone to audio.wav (16 kHz mono).
        #[arg(long)]
        mic: bool,
        /// Microphone dshow device name (default: the AMD array, else the first audio device).
        #[arg(long)]
        mic_device: Option<String>,
        /// `all` or a monitor index (see meta.json after a run).
        #[arg(long, default_value = "all")]
        monitors: String,
        /// Frames per second on the constant timeline.
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// Planor trace to copy into the recording instead of %LOCALAPPDATA%\planor\trace.log
        /// (an isolated test daemon writes its trace elsewhere).
        #[arg(long)]
        trace: Option<PathBuf>,
        /// Write this text as a marker at the start (a test name).
        #[arg(long)]
        mark: Option<String>,
        /// Pipe name suffix, so `stop`/`mark --pipe <name>` can address this recorder when several run.
        #[arg(long)]
        pipe: Option<String>,
        /// Do not register the global hotkeys (a test recorder next to a manual one).
        #[arg(long)]
        no_hotkeys: bool,
        /// Stop cleanly once the video/audio files reach this size.
        #[arg(long)]
        max_size_mb: Option<u64>,
        /// Stop cleanly after this many minutes.
        #[arg(long)]
        max_minutes: Option<f64>,
        /// x264 preset. ultrafast is the default because veryfast cannot keep up with three monitors of live
        /// motion on this machine (it drops frames); veryfast makes files about half the size.
        #[arg(long, default_value = "ultrafast")]
        preset: String,
        /// Threads per FFmpeg encoder (0 = x264 decides: about one per core and much more memory).
        #[arg(long, default_value_t = 3)]
        encoder_threads: u32,
    },
    /// Stop a running recorder.
    Stop {
        /// Pipe name suffix given to `record --pipe`.
        #[arg(long)]
        pipe: Option<String>,
    },
    /// Add a timeline marker: `planor-rec mark "window stuck"`.
    Mark {
        /// Pipe name suffix given to `record --pipe`.
        #[arg(long)]
        pipe: Option<String>,
        /// Marker text (joined with spaces).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// Analyze a recording: writes report.json, report.md and suspects/.
    Analyze {
        dir: PathBuf,
        /// Planor trace to use instead of the one in the recording or in %LOCALAPPDATA%.
        #[arg(long)]
        trace: Option<PathBuf>,
    },
    /// Copy only the flagged suspect crops (no video) into <dir>\review\ with a manifest and a narrow
    /// prompt for a Haiku subagent. Never sends anything anywhere.
    ReviewPack {
        dir: PathBuf,
        /// Most images in the pack.
        #[arg(long, default_value_t = 12)]
        max_images: usize,
        /// Also include `unmeasurable` suspects (failures only by default).
        #[arg(long)]
        include_unmeasurable: bool,
    },
    /// Print where the outline fit finds a rectangle in each frame between two times (a diagnostic).
    Probe {
        dir: PathBuf,
        #[arg(long)]
        monitor: u32,
        /// Desktop rectangle `x,y,w,h` to look for.
        #[arg(long, allow_hyphen_values = true)]
        rect: String,
        /// Milliseconds since the recording start.
        #[arg(long)]
        from_ms: i64,
        #[arg(long)]
        to_ms: i64,
        /// Also print the wallpaper/window runs along this desktop y row.
        #[arg(long, allow_hyphen_values = true)]
        row: Option<i32>,
    },
    /// Delete the oldest recordings in a folder, keeping the newest N.
    Prune {
        /// Folder that contains recording folders (each with a meta.json).
        dir: PathBuf,
        #[arg(long)]
        keep: usize,
        /// Only list what would be deleted.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a continuous low-cost rolling video/event ring buffer (default 10 s @ 30 fps).
    Ring {
        /// Rolling window duration in seconds (2..120).
        #[arg(long, default_value_t = 10)]
        seconds: u32,
        /// Capture frame rate on the ring timeline.
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// `all` or a monitor index.
        #[arg(long, default_value = "all")]
        monitors: String,
        /// x264 preset.
        #[arg(long, default_value = "ultrafast")]
        preset: String,
        /// Threads per FFmpeg encoder.
        #[arg(long, default_value_t = 2)]
        encoder_threads: u32,
        /// Pipe name suffix (defaults to `ring`, listening on `\\.\pipe\planor-rec-ring`).
        #[arg(long)]
        pipe: Option<String>,
        /// Optional trace file to tail into dumps (defaults to `%LOCALAPPDATA%\planor\trace.log`).
        #[arg(long)]
        trace: Option<PathBuf>,
        /// Scratch directory for temporary rolling TS segments.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Dump the active ring buffer window into `<out>` (`video-N.mp4`, `frames-N.csv`, `events.jsonl`, `meta.json`).
    Dump {
        /// Target folder for the dumped clip.
        #[arg(long)]
        out: PathBuf,
        /// Reason tag recorded in `meta.json` and `events.jsonl`.
        #[arg(long)]
        reason: Option<String>,
        /// Pipe name suffix (defaults to `ring`).
        #[arg(long)]
        pipe: Option<String>,
    },
    /// Query live CPU, memory, disk and frame stats of a running `planor-rec ring`.
    RingStatus {
        /// Pipe name suffix (defaults to `ring`).
        #[arg(long)]
        pipe: Option<String>,
    },
}

fn unreachable_recorder(pipe_name: &Option<String>, e: std::io::Error) -> Box<dyn std::error::Error + Send + Sync> {
    util::fmt_err(format!(
        "could not reach a recorder on {}: {e}",
        pipe::pipe_path(pipe_name.as_deref())
    ))
}

fn main() {
    capture::dpi_aware();
    let result = match Cli::parse().command {
        Command::Record {
            out,
            mic,
            mic_device,
            monitors,
            fps,
            trace,
            mark,
            pipe,
            no_hotkeys,
            max_size_mb,
            max_minutes,
            preset,
            encoder_threads,
        } => record::run(record::RecordArgs {
            out,
            mic,
            mic_device,
            monitors,
            fps,
            trace,
            mark,
            pipe,
            no_hotkeys,
            max_size_mb,
            max_minutes,
            preset,
            encoder_threads,
        }),
        Command::Stop { pipe } => pipe::send("stop", pipe.as_deref())
            .map(|reply| println!("{reply}"))
            .map_err(|e| unreachable_recorder(&pipe, e)),
        Command::Mark { pipe, text } => {
            let text = text.join(" ");
            pipe::send(&format!("mark {text}"), pipe.as_deref())
                .map(|reply| println!("{reply}"))
                .map_err(|e| unreachable_recorder(&pipe, e))
        }
        Command::Analyze { dir, trace } => analyze::run(&dir, trace.as_deref()),
        Command::ReviewPack {
            dir,
            max_images,
            include_unmeasurable,
        } => review::run_with(&dir, max_images, include_unmeasurable),
        Command::Probe {
            dir,
            monitor,
            rect,
            from_ms,
            to_ms,
            row,
        } => {
            let parts: Vec<i32> = rect.split(",").filter_map(|v| v.trim().parse().ok()).collect();
            if parts.len() != 4 {
                Err(util::fmt_err("--rect must be x,y,w,h"))
            } else {
                analyze::probe(
                    &dir,
                    monitor,
                    planor_rec::Rect::new(parts[0], parts[1], parts[2], parts[3]),
                    from_ms,
                    to_ms,
                    row,
                )
            }
        }
        Command::Prune { dir, keep, dry_run } => prune::run(&dir, keep, dry_run),
        Command::Ring {
            seconds,
            fps,
            monitors,
            preset,
            encoder_threads,
            pipe,
            trace,
            dir,
        } => ring::run(ring::RingArgs {
            seconds,
            fps,
            monitors,
            preset,
            encoder_threads,
            pipe,
            trace,
            dir,
        }),
        Command::Dump { out, reason, pipe } => {
            ring::send_dump(&out, reason.as_deref(), pipe.as_deref())
        }
        Command::RingStatus { pipe } => ring::send_status(pipe.as_deref()),
    };
    if let Err(e) = result {
        eprintln!("planor-rec: {e}");
        std::process::exit(1);
    }
}
