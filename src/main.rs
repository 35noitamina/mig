mod absorb;
mod agent;
#[cfg(feature = "gui")]
mod agentsui;
mod ai;
#[cfg(feature = "gui")]
mod aiui;
mod approval;
#[cfg(feature = "gui")]
mod approvalsui;
mod assist;
mod chart;
mod cli;
mod config;
#[cfg(feature = "gui")]
mod consoleui;
#[cfg(feature = "gui")]
mod editor;
mod events;
mod files;
#[cfg(feature = "gui")]
mod finder;
mod gc;
mod github;
mod growth;
mod guard;
#[cfg(feature = "gui")]
mod gui;
#[cfg(feature = "gui")]
mod highlight;
mod inbox;
mod jobs;
mod live;
#[cfg(feature = "gui")]
mod liveui;
mod livewatch;
mod lock;
mod mcp;
mod oplog;
mod ops;
mod outfmt;
mod outlog;
mod patch;
mod repo;
mod rev;
mod runcheck;
mod runner;
mod runtool;
mod scan;
#[cfg(feature = "gui")]
mod settings;
mod setup;
#[cfg(feature = "gui")]
mod sizeui;
mod split;
#[cfg(feature = "gui")]
mod tellui;
#[cfg(feature = "gui")]
mod viewer;
#[cfg(feature = "gui")]
mod welcome;
mod workspace;

fn main() {
  if let Err(e) = cli::run() {
    eprintln!("error: {e:#}");
    std::process::exit(1);
  }
}
