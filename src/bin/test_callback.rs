//! Test fixture: portable helpers for integration tests.
//!
//! Subcommands:
//! ```text
//! touch <path>      — create an empty file at <path>
//! echo-env <path>   — write "$TENDER_SESSION $TENDER_NAMESPACE $TENDER_EXIT_REASON" to <path>
//! print-cwd         — print the current working directory to stdout
//! ```

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: test_callback <touch|echo-env|print-cwd> [<path>]");
        std::process::exit(1);
    }

    let cmd = &args[1];

    match cmd.as_str() {
        "touch" | "echo-env" => {
            if args.len() < 3 {
                eprintln!("usage: test_callback {cmd} <path>");
                std::process::exit(1);
            }
            let path = &args[2];
            match cmd.as_str() {
                "touch" => {
                    std::fs::write(path, "").unwrap_or_else(|e| {
                        eprintln!("test_callback touch: {e}");
                        std::process::exit(1);
                    });
                }
                "echo-env" => {
                    let session = std::env::var("TENDER_SESSION").unwrap_or_default();
                    let namespace = std::env::var("TENDER_NAMESPACE").unwrap_or_default();
                    let exit_reason = std::env::var("TENDER_EXIT_REASON").unwrap_or_default();
                    let content = format!("{session} {namespace} {exit_reason}\n");
                    std::fs::write(path, content).unwrap_or_else(|e| {
                        eprintln!("test_callback echo-env: {e}");
                        std::process::exit(1);
                    });
                }
                _ => unreachable!(),
            }
        }
        "print-cwd" => {
            let cwd = std::env::current_dir().unwrap_or_else(|e| {
                eprintln!("test_callback print-cwd: {e}");
                std::process::exit(1);
            });
            println!("{}", cwd.display());
        }
        other => {
            eprintln!("test_callback: unknown command '{other}'");
            std::process::exit(1);
        }
    }
}
