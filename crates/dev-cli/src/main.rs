//! paperanywhere-dev (binary name: `pa-dev`) — developer CLI.
//!
//! Wraps the existing flashing / provisioning tools so the developer
//! runs one tool from a checkout instead of remembering espflash flags
//! or provtool environment variables.
//!
//! ```text
//! pa-dev provision --ssid mynet --pass secret --backend http://10.0.1.5:8080 [--dev]
//! pa-dev flash                # cargo build --profile release-dev + espflash
//! pa-dev monitor              # espflash monitor on the connected device
//! ```
//!
//! `pa-dev push` (wireless firmware iteration via a backend-instructed
//! OTA — see task #93) is not yet wired. Until that lands, use
//! `pa-dev flash` over USB for every iteration.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "pa-dev",
    version,
    about = "Developer CLI for paperanywhere firmware iteration."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Generate a prov.bin for the firmware's prov partition.
    Provision(ProvisionArgs),
    /// Build the firmware via the docker espressif/idf-rust container
    /// and flash it over USB serial.
    Flash(FlashArgs),
    /// Tail serial output from the connected device.
    Monitor(MonitorArgs),
    /// Erase the NVS partition over serial so the next boot migrates
    /// fresh credentials from the prov partition. Use when WiFi creds
    /// were updated but the device's NVS cache still has the old ones.
    FactoryReset(FactoryResetArgs),
}

#[derive(Args, Debug)]
struct ProvisionArgs {
    /// WiFi SSID to bake into the prov bundle.
    #[arg(long, env = "PAPERANYWHERE_PROV_SSID")]
    ssid: String,
    /// WiFi password.
    #[arg(long, env = "PAPERANYWHERE_PROV_PASS")]
    pass: String,
    /// Backend URL the device should poll.
    #[arg(long, env = "PAPERANYWHERE_PROV_BACKEND_URL")]
    backend: String,
    /// Mark this bundle as a dev build — device skips production OTA
    /// scheduling and stays AlwaysOn so backend-instructed OTA pushes
    /// (task #93) can reach it without waiting for the next wake.
    #[arg(long)]
    dev: bool,
    /// Where to write the prov.bin.
    #[arg(long, short = 'o', default_value = "prov.bin")]
    output: PathBuf,
    /// Skip the flash step — just emit the .bin and exit.
    #[arg(long)]
    no_flash: bool,
    /// Serial port to flash to.
    #[arg(long, env = "PAPERANYWHERE_PROV_PORT", default_value = "COM6")]
    port: String,
}

#[derive(Args, Debug)]
struct FlashArgs {
    /// Cargo feature flag for the target board.
    #[arg(long, default_value = "board-reterminal-e1001")]
    board: String,
    /// Use the full LTO `release` profile instead of `release-dev`.
    /// Slower to build, smaller binary.
    #[arg(long)]
    release: bool,
    /// Skip the cargo build and flash the previous artifact as-is.
    #[arg(long)]
    no_build: bool,
    /// Serial port for the flash.
    #[arg(long, default_value = "COM6")]
    port: String,
    /// After flashing, attach the serial monitor.
    #[arg(long)]
    monitor: bool,
}

#[derive(Args, Debug)]
struct MonitorArgs {
    /// Serial port.
    #[arg(long, default_value = "COM6")]
    port: String,
}

#[derive(Args, Debug)]
struct FactoryResetArgs {
    /// Serial port the device is attached to.
    #[arg(long, default_value = "COM6")]
    port: String,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Provision(a) => run_provision(a),
        Cmd::Flash(a) => run_flash(a),
        Cmd::Monitor(a) => run_monitor(a),
        Cmd::FactoryReset(a) => run_factory_reset(a),
    }
}

fn run_factory_reset(args: FactoryResetArgs) -> Result<()> {
    // espflash 4.x's `erase-region` takes (offset, size) and clears
    // the underlying flash. NVS lives at 0x9000, size 24 KB (0x6000),
    // matching crates/firmware/flash/partition-table.csv.
    println!("pa-dev: erasing NVS region (0x9000, 24 KB) on {}", args.port);
    let status = Command::new(espflash_bin())
        .args(["erase-region", "--port", &args.port, "0x9000", "0x6000"])
        .status()
        .context("invoke espflash erase-region")?;
    if !status.success() {
        bail!("espflash erase-region failed: exit {:?}", status.code());
    }
    println!(
        "pa-dev: NVS wiped. On next boot the firmware will re-migrate \
         from the prov partition. Flash a fresh prov.bin via `pa-dev \
         provision` if you also want to update WiFi creds."
    );
    Ok(())
}

// ── Provision ───────────────────────────────────────────────────────────────

fn run_provision(args: ProvisionArgs) -> Result<()> {
    println!("pa-dev: generating prov.bin (dev={})", args.dev);
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "-p", "paperanywhere-provtool", "--quiet", "--"]);
    cmd.arg("--output").arg(&args.output);
    if args.dev {
        cmd.arg("--dev");
    }
    cmd.env("PAPERANYWHERE_PROV_SSID", &args.ssid);
    cmd.env("PAPERANYWHERE_PROV_PASS", &args.pass);
    cmd.env("PAPERANYWHERE_PROV_BACKEND_URL", &args.backend);
    let status = cmd.status().context("invoke cargo run -p provtool")?;
    if !status.success() {
        bail!("provtool failed: exit {:?}", status.code());
    }

    if args.no_flash {
        return Ok(());
    }
    println!("pa-dev: flashing prov.bin to {}", args.port);
    let status = Command::new(espflash_bin())
        .args(["write-bin", "--port", &args.port, "0x12000"])
        .arg(&args.output)
        .status()
        .context("invoke espflash write-bin")?;
    if !status.success() {
        bail!("espflash write-bin failed: exit {:?}", status.code());
    }
    Ok(())
}

// ── Flash (firmware) ────────────────────────────────────────────────────────

fn run_flash(args: FlashArgs) -> Result<()> {
    let profile = if args.release { "release" } else { "release-dev" };
    let elf = format!(
        "target/xtensa-esp32s3-none-elf/{}/paperanywhere-firmware",
        profile
    );

    if !args.no_build {
        println!("pa-dev: building firmware ({}, {})", args.board, profile);
        // The xtensa toolchain lives inside espressif/idf-rust:esp32s3_latest.
        // Run via docker so the developer doesn't need esp-rs installed on
        // their host. If `docker` isn't available we fall back to a plain
        // cargo build assuming the user has espup configured.
        let docker_ok = Command::new("docker").arg("--version").status();
        let built = if docker_ok.is_ok() && docker_ok.unwrap().success() {
            run_docker_build(&args.board, profile)
        } else {
            run_native_cargo_build(&args.board, profile)
        };
        built.context("firmware build")?;
    }

    println!("pa-dev: flashing {} via {}", elf, args.port);
    let status = Command::new(espflash_bin())
        .args(["flash", "--port", &args.port])
        .args([
            "--partition-table",
            "crates/firmware/flash/partition-table.csv",
        ])
        .arg(&elf)
        .status()
        .context("invoke espflash flash")?;
    if !status.success() {
        bail!("espflash flash failed: exit {:?}", status.code());
    }

    if args.monitor {
        run_monitor(MonitorArgs { port: args.port })?;
    }
    Ok(())
}

fn run_docker_build(board: &str, profile: &str) -> Result<()> {
    let pwd = std::env::current_dir()?;
    let pwd_str = pwd
        .to_str()
        .ok_or_else(|| anyhow!("cwd not UTF-8: {}", pwd.display()))?;
    let cargo_cmd = format!(
        "source /home/esp/export-esp.sh && cargo build -p paperanywhere-firmware \
         --no-default-features --features {} --profile {} \
         --target xtensa-esp32s3-none-elf -Z build-std=core,alloc",
        board, profile
    );
    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/work", pwd_str),
            "-w",
            "/work",
            "espressif/idf-rust:esp32s3_latest",
            "bash",
            "-lc",
            &cargo_cmd,
        ])
        .status()
        .context("docker run")?;
    if !status.success() {
        bail!("docker firmware build failed: exit {:?}", status.code());
    }
    Ok(())
}

fn run_native_cargo_build(board: &str, profile: &str) -> Result<()> {
    let status = Command::new("cargo")
        .args(["build", "-p", "paperanywhere-firmware"])
        .args(["--no-default-features", "--features", board])
        .args(["--profile", profile])
        .args(["--target", "xtensa-esp32s3-none-elf"])
        .args(["-Z", "build-std=core,alloc"])
        .status()
        .context("invoke cargo build")?;
    if !status.success() {
        bail!("cargo build failed: exit {:?}", status.code());
    }
    Ok(())
}

// ── Monitor ─────────────────────────────────────────────────────────────────

fn run_monitor(args: MonitorArgs) -> Result<()> {
    let status = Command::new(espflash_bin())
        .args(["monitor", "--port", &args.port])
        .status()
        .context("invoke espflash monitor")?;
    if !status.success() {
        bail!("espflash monitor failed: exit {:?}", status.code());
    }
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn espflash_bin() -> String {
    // Prefer the one in cwd (the firmware repo ships a Windows .exe for
    // contributors who don't have it on PATH); fall back to PATH.
    if cfg!(windows) {
        if std::path::Path::new("espflash.exe").exists() {
            return "./espflash.exe".to_string();
        }
        if std::path::Path::new(".\\espflash.exe").exists() {
            return ".\\espflash.exe".to_string();
        }
    }
    "espflash".to_string()
}
