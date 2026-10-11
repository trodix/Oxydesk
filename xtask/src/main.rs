//! `cargo xtask vm-test`: orchestrates build → disposable VM → test client → report.
//! `cargo xtask test`: unit tests, plus the slow Windows integration test with `--integration`.

mod opts;
mod report;
mod runner;
mod testcmd;

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
        Some("test") => match testcmd::parse(args.collect(), |k| std::env::var(k).ok()) {
            Ok(testcmd::TestParsed::Help) => {
                println!("{}", testcmd::HELP);
                ExitCode::SUCCESS
            }
            Ok(testcmd::TestParsed::Run(o)) => testcmd::run(&o),
            Err(e) => {
                eprintln!("{e}\n\n{}", testcmd::HELP);
                ExitCode::from(2)
            }
        },
        Some("host-check") => testcmd::host_check(args.collect()),
        _ => {
            eprintln!("{}\n\n{}", opts::HELP, testcmd::HELP);
            ExitCode::from(2)
        }
    }
}
