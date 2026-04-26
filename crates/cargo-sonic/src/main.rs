use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargo-sonic", bin_name = "cargo-sonic")]
struct Cli {
    #[command(subcommand)]
    command: CargoSonicCommand,
}

#[derive(Subcommand)]
enum CargoSonicCommand {
    Sonic(Sonic),
}

#[derive(Parser)]
struct Sonic {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Build(Build),
}

#[derive(Parser)]
struct Build {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    cargo_args: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CargoSonicCommand::Sonic(Sonic {
            command: Command::Build(build),
        }) => sonic_build::build(sonic_build::BuildOptions {
            cargo_args: build.cargo_args,
            manifest_path: None,
        })
        .map(|output| {
            println!("{}", output.final_binary);
        }),
    }
    .map_err(|err| {
        if err.downcast_ref::<clap::Error>().is_some() {
            err
        } else {
            anyhow::anyhow!("{err:#}")
        }
    })
    .or_else(|err| {
        bail!("{err}");
    })
}
