use std::io::{self, ErrorKind};
use std::process::ExitCode;

use ff::app::Outcome;
use ff::cli::{self, Command};

fn main() -> ExitCode {
    let cfg = match cli::parse(std::env::args().skip(1)) {
        Ok(Command::Run(cfg)) => cfg,
        Ok(Command::Help) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("ff {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Command::Init(shell)) => {
            return match ff::shell::script(&shell) {
                Some(script) => {
                    print!("{script}");
                    ExitCode::SUCCESS
                }
                None => {
                    eprintln!("ff: unsupported shell '{shell}' (supported: {})", ff::shell::SHELLS.join(", "));
                    ExitCode::from(2)
                }
            };
        }
        Err(e) => {
            eprintln!("ff: {e}\nTry 'ff --help' for more information.");
            return ExitCode::from(2);
        }
    };

    if !cfg.root.is_dir() {
        eprintln!("ff: '{}' is not a directory", cfg.root.display());
        return ExitCode::from(2);
    }

    if let Some(query) = &cfg.filter {
        return match ff::run_filter(&cfg, query, &mut io::stdout().lock()) {
            Ok(0) => ExitCode::from(1),
            Ok(_) => ExitCode::SUCCESS,
            // e.g. `ff --filter x | head`
            Err(e) if e.kind() == ErrorKind::BrokenPipe => ExitCode::SUCCESS,
            // e.g. an invalid regex
            Err(e) => {
                eprintln!("ff: {e}");
                ExitCode::from(2)
            }
        };
    }

    // Exit codes follow fzf: 0 selected, 130 cancelled, 2 error.
    match ff::tui::run(&cfg) {
        Ok(Outcome::Accept(selection)) => {
            println!("{selection}");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Open(targets)) => match ff::editor::open(&targets) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ff: {e}");
                ExitCode::from(2)
            }
        },
        Ok(_) => ExitCode::from(130),
        Err(e) => {
            eprintln!("ff: {e}");
            ExitCode::from(2)
        }
    }
}
