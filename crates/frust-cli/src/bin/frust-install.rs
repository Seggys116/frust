use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use frust_cli::install;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("usage: frust-install <install|upgrade|stage|rollback|uninstall> --prefix DIR [--archive FILE --checksum FILE]");
        return ExitCode::from(2);
    };
    let mut prefix = None;
    let mut archive = None;
    let mut checksum = None;
    while let Some(arg) = args.next() {
        let value = args.next();
        match arg.as_str() {
            "--prefix" => prefix = value.map(PathBuf::from),
            "--archive" => archive = value.map(PathBuf::from),
            "--checksum" => checksum = value.map(PathBuf::from),
            other => {
                eprintln!("unknown argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(prefix) = prefix else {
        eprintln!("--prefix is required");
        return ExitCode::from(2);
    };
    let result = match command.as_str() {
        "install" | "upgrade" => {
            let (Some(archive), Some(checksum)) = (archive, checksum) else {
                eprintln!("install and upgrade require --archive and --checksum");
                return ExitCode::from(2);
            };
            install::install(&prefix, &archive, &checksum).map(|state| format!("current={}\n", state.current.unwrap_or_default()))
        }
        "stage" => {
            let (Some(archive), Some(checksum)) = (archive, checksum) else {
                eprintln!("stage requires --archive and --checksum");
                return ExitCode::from(2);
            };
            install::stage_without_activate(&prefix, &archive, &checksum).map(|path| {
                let current = install::current_version(&prefix).ok().flatten().unwrap_or_default();
                format!("partial={}\ncurrent={current}\n", path.display())
            })
        }
        "rollback" => install::rollback(&prefix).map(|state| format!("current={}\n", state.current.unwrap_or_default())),
        "uninstall" => install::uninstall(&prefix).map(|()| String::new()),
        other => {
            eprintln!("unknown command {other}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {}", err.message);
            ExitCode::from(1)
        }
    }
}
