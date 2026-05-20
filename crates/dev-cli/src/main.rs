//! paperanywhere-dev (binary name: `pa-dev`) — developer CLI.
//!
//! Wraps the existing flashing / provisioning tools and adds an HTTP
//! client for the device-side firmware-PUT endpoint (task #79 once
//! that lands). Goal: one tool the developer runs from a checkout to
//! do every iteration step without remembering espflash flags or
//! provtool environment variables.
//!
//! ```text
//! pa-dev provision --ssid mynet --pass secret --backend http://10.0.1.5:8080 [--dev]
//! pa-dev flash                # cargo build --profile release-dev + espflash
//! pa-dev monitor              # espflash monitor on the connected device
//! pa-dev push --device 10.0.1.42 path/to/firmware.bin
//! pa-dev info --device 10.0.1.42
//! ```

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use sha2::{Digest, Sha256};

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
    /// PUT a firmware .bin to a dev device over HTTP — no cable
    /// required. Only works against firmware flashed with `--dev`
    /// (because only dev builds run the receiver). See task #79.
    Push(PushArgs),
    /// GET the dev device's /info endpoint and pretty-print the JSON
    /// (firmware version, board, channel, IP, etc.).
    Info(InfoArgs),
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
    /// and runs the dev-only firmware PUT receiver (when #79 lands).
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
struct PushArgs {
    /// Device address — IP or hostname. Read from the boot-screen
    /// status bar or `pa-dev info` discovery.
    #[arg(long)]
    device: String,
    /// HTTP port the device's firmware-PUT receiver listens on.
    #[arg(long, default_value_t = 80)]
    port: u16,
    /// Path to the firmware .bin to upload.
    bin: PathBuf,
    /// Skip the sha256 check (device default rejects unverified
    /// pushes; opt-out for local debugging only).
    #[arg(long)]
    skip_verify: bool,
}

#[derive(Args, Debug)]
struct InfoArgs {
    #[arg(long)]
    device: String,
    #[arg(long, default_value_t = 80)]
    port: u16,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Provision(a) => run_provision(a),
        Cmd::Flash(a) => run_flash(a),
        Cmd::Monitor(a) => run_monitor(a),
        Cmd::Push(a) => run_push(a),
        Cmd::Info(a) => run_info(a),
    }
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

// ── Push (HTTP) ─────────────────────────────────────────────────────────────

fn run_push(args: PushArgs) -> Result<()> {
    let bytes = std::fs::read(&args.bin)
        .with_context(|| format!("read {}", args.bin.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha = hex_encode(&hasher.finalize());

    let url = format!("http://{}:{}/firmware", args.device, args.port);
    println!(
        "pa-dev: PUT {} ({} bytes, sha256 {})",
        url,
        bytes.len(),
        &sha[..16]
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let mut req = client
        .put(&url)
        .body(bytes)
        .header("Content-Type", "application/octet-stream");
    if !args.skip_verify {
        req = req.header("X-PA-Sha256", &sha);
    }
    let resp = req.send().context("PUT request failed (is the device on the dev channel and reachable?)")?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    println!("pa-dev: {} {}", status, body);
    if !status.is_success() {
        bail!("device rejected the upload");
    }
    Ok(())
}

// ── Info ────────────────────────────────────────────────────────────────────

fn run_info(args: InfoArgs) -> Result<()> {
    let url = format!("http://{}:{}/info", args.device, args.port);
    let resp = reqwest::blocking::get(&url).context("GET /info failed")?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("device returned {}: {}", status, body);
    }
    // Pretty-print if it's JSON; passthrough otherwise.
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
        Err(_) => println!("{}", body),
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

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(nibble(b >> 4));
        s.push(nibble(b & 0xF));
    }
    s
}

fn nibble(n: u8) -> char {
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'a' + n - 10) as char
    }
}
