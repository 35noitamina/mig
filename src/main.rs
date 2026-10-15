mod cli;
#[cfg(feature = "gui")]
mod gui;
mod oplog;
mod ops;
mod repo;
mod rev;
mod scan;

fn main() {
  if let Err(e) = cli::run() {
    eprintln!("error: {e:#}");
    std::process::exit(1);
  }
}
