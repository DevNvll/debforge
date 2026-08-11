use std::process::ExitCode;

fn main() -> ExitCode {
    match debtap_rs::privileged::run(std::env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}
