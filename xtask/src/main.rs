//! `cargo xtask vm-test`: orchestrates build → disposable VM → test client → report.

mod opts;
mod report;
mod runner;

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("vm-test") => match opts::parse(args.collect(), |k| std::env::var(k).ok()) {
            Ok(opts::Parsed::Help) => {
                println!("{}", opts::HELP);
                ExitCode::SUCCESS
            }
            Ok(opts::Parsed::Run(o)) => runner::vm_test(&o),
            Err(e) => {
                eprintln!("{e}\n\n{}", opts::HELP);
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("{}", opts::HELP);
            ExitCode::from(2)
        }
    }
}
