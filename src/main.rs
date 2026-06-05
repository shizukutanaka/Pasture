use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let code = pasture::cli::run(&args);
    ExitCode::from(code as u8)
}
