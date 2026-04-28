use anyhow::{Result, bail};
use cargo_sonic::{BuildOptions, ProbeOptions};
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
    #[arg(long, value_delimiter = ',')]
    target_cpus: Vec<String>,

    #[arg(short = 'p', long, default_value_t = 1)]
    parallelism: usize,

    #[arg(long)]
    compress: Option<String>,

    #[arg(long, default_value_t = 22)]
    compression_level: i32,

    #[arg(long)]
    auditable: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Build(Build),
    Probe(Probe),
}

#[derive(Parser)]
struct Build {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    cargo_args: Vec<String>,
}

#[derive(Parser)]
struct Probe {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    cargo_args: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CargoSonicCommand::Sonic(Sonic {
            target_cpus,
            parallelism,
            compress,
            compression_level,
            auditable,
            command: Command::Build(build),
        }) => cargo_sonic::build(BuildOptions {
            cargo_args: build.cargo_args,
            manifest_path: None,
            target_cpus,
            parallelism,
            compress,
            compression_level,
            auditable,
        })
        .map(|output| {
            println!("{}", output.final_binary);
        }),
        CargoSonicCommand::Sonic(Sonic {
            target_cpus,
            parallelism: _,
            compress: _,
            compression_level: _,
            auditable: _,
            command: Command::Probe(probe),
        }) => cargo_sonic::probe(ProbeOptions {
            cargo_args: probe.cargo_args,
            target_cpus,
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
