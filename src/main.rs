mod absorb;
mod agent;
mod ai;
#[cfg(feature = "gui")]
mod aiui;
mod assist;
mod cli;
mod config;
#[cfg(feature = "gui")]
mod editor;
mod files;
mod gc;
mod github;
mod growth;
mod guard;
#[cfg(feature = "gui")]
mod gui;
mod live;
#[cfg(feature = "gui")]
mod liveui;
#[cfg(feature = "gui")]
mod livewatch;
mod mcp;
mod oplog;
mod ops;
mod patch;
mod repo;
mod rev;
mod scan;
#[cfg(feature = "gui")]
mod settings;
mod setup;
#[cfg(feature = "gui")]
mod sizeui;
mod split;
#[cfg(feature = "gui")]
mod viewer;
#[cfg(feature = "gui")]
mod welcome;

fn main() {
  if let Err(e) = cli::run() {
    eprintln!("error: {e:#}");
    std::process::exit(1);
  }
}
