use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use threadpool::ThreadPool;
use usi::{
    CheckmateParams, EngineCommand, GuiCommand, InfoParams, MateParam, ThinkParams,
    UsiEngineHandler,
};

/// Engine options
#[derive(Parser, Debug, Clone, Copy)]
#[command()]
struct EngineOptions {
    /// The number of threads to use
    #[arg(short, long, default_value = "1")]
    threads: usize,

    /// The hash size in MB
    #[arg(short, long, default_value = "16")]
    hash: usize,
}

/// A benchmarking tool for mate engines
#[derive(Parser, Debug, Clone)]
#[command(version, disable_help_flag = true)]
struct Args {
    /// The path to the engine executable
    #[arg(short, long)]
    engine_path: String,

    /// The paths to the sfen files to use. It must not be empty
    #[arg()]
    sfen_paths: Vec<String>,

    /// The number of workers to use
    #[arg(short, long, default_value = "4")]
    workers: usize,

    /// The engine options
    #[command(flatten)]
    engine_options: EngineOptions,

    /// Show help message
    #[clap(long, action = clap::ArgAction::HelpLong)]
    help: Option<bool>,
}

/// Statistics for a solve
#[derive(Debug, Default, Clone)]
struct SolveStats {
    /// The number of sfens processed
    num_sfens: usize,
    /// The number of positions with a mate
    num_mate: usize,
    /// The number of positions with no mate
    num_nomate: usize,
    /// The number of positions with an error
    num_errors: usize,
    /// The time taken to solve the positions
    elapsed: Duration,
    /// The number of nodes searched
    nodes: usize,
    /// The number of nodes searched in the last n positions
    last_nodes: usize,

    /// The indices of the positions with an error or no mate
    error_or_nomate_indices: Vec<usize>,
}

impl SolveStats {
    fn update_by_checkmate(&mut self, mate: &CheckmateParams) {
        use CheckmateParams::*;

        let sfen_index = self.num_sfens;
        self.num_sfens += 1;
        self.nodes += self.last_nodes;
        self.last_nodes = 0;
        match mate {
            Mate(_) => self.num_mate += 1,
            NoMate => {
                self.num_nomate += 1;
                self.error_or_nomate_indices.push(sfen_index);
            }
            _ => {
                self.num_errors += 1;
                self.error_or_nomate_indices.push(sfen_index);
            }
        }
    }

    fn update_by_info(&mut self, info: &[InfoParams]) {
        let has_pv = info.iter().any(|x| matches!(x, InfoParams::Pv(_)));
        if has_pv {
            let nodes = info
                .iter()
                .find_map(|x| match x {
                    InfoParams::Nodes(nodes) => Some(*nodes),
                    _ => None,
                })
                .unwrap_or(0);

            self.last_nodes = nodes as usize;
        }
    }
}

/// Check the arguments
///
/// Check that the arguments are valid. If they are not, return an error.
fn check_args(args: &Args) -> Result<()> {
    if args.sfen_paths.is_empty() {
        bail!("No sfen files provided");
    }

    if args.engine_options.threads == 0 {
        bail!("Threads must be greater than 0");
    }

    Ok(())
}

/// Initialize the engine
///
/// Initialize the engine with the given path and options. If the engine fails to initialize, return an error.
fn initialize_engine(
    engine_path: &str,
    engine_options: &EngineOptions,
) -> Result<UsiEngineHandler> {
    let mut engine = UsiEngineHandler::spawn(&engine_path, ".").context("Engine spawn error")?;

    engine.send_command(&GuiCommand::SetOption(
        "Threads".to_string(),
        Some(engine_options.threads.to_string()),
    ))?;
    engine.send_command(&GuiCommand::SetOption(
        "USI_Hash".to_string(),
        Some(engine_options.hash.to_string()),
    ))?;

    let default_options = [
        ("GenerateAllLegalMoves", "true"),
        ("PvInterval", "0"),
        ("RootIsAndNodeIfChecked", "false"),
        ("PostSearchLevel", "None"),
    ];
    for (name, value) in default_options {
        engine.send_command(&GuiCommand::SetOption(
            name.to_string(),
            Some(value.to_string()),
        ))?;
    }

    engine.prepare()?;
    engine.send_command(&GuiCommand::UsiNewGame)?;
    Ok(engine)
}

/// Start searching
fn start_searching(engine: &mut UsiEngineHandler, sfen: &str) -> Result<()> {
    let setpos_cmd = GuiCommand::Position(sfen.to_string());
    let mate_cmd =
        GuiCommand::Go(ThinkParams::new().mate(MateParam::Timeout(Duration::from_secs(30))));

    engine.send_command(&setpos_cmd)?;
    engine.send_command(&mate_cmd)?;

    Ok(())
}

/// Get the progress style
fn get_style() -> Result<ProgressStyle> {
    let style = ProgressStyle::with_template(
        "({elapsed_precise})[{eta:>5}] {bar:40.cyan/blue} {pos:>6}/{len:7} {msg}",
    )?
    .progress_chars("##-");

    Ok(style)
}

/// Solve the positions
///
/// Solve the positions in the given sfen file with the given engine. Return the statistics for the solve.
fn solve<'a>(
    engine_path: &str,
    engine_options: &EngineOptions,
    sfen_path: &str,
    progress: &MultiProgress,
) -> Result<SolveStats> {
    // <progress_bar> prepare progress bar
    let num_sfens = BufReader::new(File::open(sfen_path)?).lines().count();
    let progress_bar = progress.add(ProgressBar::new(num_sfens as u64));
    let sfen_file_name = Path::new(sfen_path)
        .file_name()
        .context("Invalid sfen path")?;
    progress_bar.set_style(get_style()?);
    progress_bar.set_message(sfen_file_name.to_string_lossy().to_string());
    let progress_bar = Arc::new(progress_bar);
    // </progress_bar>

    let mut engine = initialize_engine(engine_path, engine_options)?;

    let solve_stats = Arc::new(Mutex::new(SolveStats::default()));
    let solve_stats_clone = solve_stats.clone();
    let progress_bar_clone = progress_bar.clone();
    engine.listen(move |output| -> Result<(), usi::Error> {
        use EngineCommand::*;

        match output.response() {
            Some(Checkmate(mate)) => {
                progress_bar_clone.inc(1);

                let mut solve_stats = solve_stats_clone.lock().unwrap();
                solve_stats.update_by_checkmate(mate)
            }
            Some(Info(info)) => {
                let mut solve_stats = solve_stats_clone.lock().unwrap();
                solve_stats.update_by_info(&info);
            }
            _ => {}
        }
        Ok(())
    })?;

    let start_instant = Instant::now();
    for sfen in BufReader::new(File::open(sfen_path)?).lines() {
        start_searching(&mut engine, &sfen?)?;
    }

    // wait until all sfens are processed
    while solve_stats.lock().unwrap().num_sfens < num_sfens {
        thread::sleep(Duration::from_millis(100));
    }
    let end_instant = Instant::now();

    progress_bar.finish();
    progress.remove(&progress_bar);
    let mut solve_stats = solve_stats.lock().unwrap().clone();
    solve_stats.elapsed = end_instant - start_instant;
    Ok(solve_stats)
}

/// Print the statistics
fn print_stats(sfen_path: &str, solve_stats: &SolveStats) {
    println!(
        "[{:>48}:{:>6.1}s] nps: {:10.2}, nodes: {:10}, pos: {:6}",
        sfen_path,
        solve_stats.elapsed.as_secs_f64(),
        solve_stats.nodes as f64 / solve_stats.elapsed.as_secs_f64(),
        solve_stats.nodes,
        solve_stats.num_sfens,
    );
    if solve_stats.num_nomate > 0 {
        println!("  Nomate: {}", solve_stats.num_nomate);
    }
    if solve_stats.num_errors > 0 {
        println!("  Errors: {}", solve_stats.num_errors);
    }
    if !solve_stats.error_or_nomate_indices.is_empty() {
        // take first 10 element
        let mut error_or_nomate_indices = solve_stats.error_or_nomate_indices.clone();
        let is_too_many_indices = error_or_nomate_indices.len() > 10;
        if is_too_many_indices {
            error_or_nomate_indices.truncate(10);
        }
        let mut error_or_nomate_indices = error_or_nomate_indices
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        if is_too_many_indices {
            error_or_nomate_indices.push_str(", ...");
        }

        println!("  Error or Nomate indices: {}", error_or_nomate_indices,);
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    check_args(&args)?;

    let progress = Arc::from(MultiProgress::new());

    let (tx, rx) = mpsc::channel();
    let thread_pool = ThreadPool::new(args.workers);
    for sfen_path in args.sfen_paths {
        let engine_path = args.engine_path.clone();
        let engine_options = args.engine_options.clone();
        let progress = progress.clone();
        let tx = tx.clone();
        thread_pool.execute(move || {
            let solve_stats =
                solve(&engine_path, &engine_options, &sfen_path, &progress).unwrap_or_default();
            tx.send((sfen_path, solve_stats)).unwrap();
        });
    }

    drop(tx);
    let mut total_nodes = 0;
    for (sfen_path, solve_stats) in rx.iter() {
        total_nodes += solve_stats.nodes;
        progress.suspend(|| print_stats(&sfen_path, &solve_stats));
    }

    thread_pool.join();
    progress.clear()?;
    println!("Total nodes: {}", total_nodes);

    Ok(())
}
