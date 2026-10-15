mod absorb;
mod cli;
mod config;
#[cfg(feature = "gui")]
mod editor;
mod gc;
mod guard;
#[cfg(feature = "gui")]
mod gui;
mod oplog;
mod ops;
mod patch;
mod repo;
mod rev;
mod scan;
mod split;

fn main() {
  if let Err(e) = cli::run() {
    eprintln!("error: {e:#}");
    std::process::exit(1);
  }
}
