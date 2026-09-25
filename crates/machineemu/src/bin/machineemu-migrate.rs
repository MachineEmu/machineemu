//! One-time conversion of repository or workspace documents to the v1 domain
//! YAML layout. The daemon intentionally does not invoke this automatically:
//! operators run it during a maintenance window after taking a backup.

use machineemu_core::storage::{MigrationOptions, migrate_legacy_tree};
use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let mut apply = false;
    let mut remove_legacy = false;
    let mut root = None;
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--apply" => apply = true,
            "--remove-legacy" => remove_legacy = true,
            "--help" | "-h" => {
                println!("usage: machineemu-migrate [--apply] [--remove-legacy] ROOT");
                println!("without --apply, only preflight conversion is performed");
                return ExitCode::SUCCESS;
            }
            value if value.starts_with('-') => {
                eprintln!("unknown option: {value}");
                return ExitCode::from(2);
            }
            value => {
                if root.replace(value.to_owned()).is_some() {
                    eprintln!("only one ROOT is accepted");
                    return ExitCode::from(2);
                }
            }
        }
    }
    let Some(root) = root else {
        eprintln!("ROOT is required; use --help for usage");
        return ExitCode::from(2);
    };
    match migrate_legacy_tree(
        root,
        MigrationOptions {
            apply,
            remove_legacy,
        },
    ) {
        Ok(report) => {
            println!("{} entries processed", report.entries.len());
            for entry in report.entries {
                println!(
                    "{} -> {} ({})",
                    entry.source, entry.destination, entry.disposition
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("migration failed: {error}");
            ExitCode::from(1)
        }
    }
}
