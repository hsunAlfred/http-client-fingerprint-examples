use std::{fs::OpenOptions, io::Write, process::ExitCode};

use clap::{Parser, error::ErrorKind};
use fingerprint_client_rs::{Args, RunError, run};

#[tokio::main]
async fn main() -> ExitCode {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error) => {
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                return if error.print().is_ok() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(2)
                };
            }
            // clap's usual error can echo the original URL or other sensitive argv.
            let _ = writeln!(std::io::stderr(), "{}", RunError::Arguments);
            return ExitCode::from(2);
        }
    };
    match execute(&args).await {
        Ok(failed) => ExitCode::from(u8::from(failed)),
        Err(error) => {
            // Display is a fixed safe message. Do not log Debug or the source chain.
            let _ = writeln!(std::io::stderr(), "{error}");
            ExitCode::from(2)
        }
    }
}

async fn execute(args: &Args) -> Result<bool, RunError> {
    args.validate()?;
    // Open before networking; create_new prevents accidental overwrite.
    let mut output: Box<dyn Write> = if args.output == "-" {
        Box::new(std::io::stdout())
    } else {
        Box::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&args.output)
                .map_err(RunError::Output)?,
        )
    };
    let summary = run(args, &mut output).await?;
    let encoded = serde_json::to_vec(&summary).map_err(RunError::Serialization)?;
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(&encoded).map_err(RunError::Output)?;
    stderr.write_all(b"\n").map_err(RunError::Output)?;
    Ok(summary.failed != 0)
}
