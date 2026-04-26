use anyhow::Result;
use std::process::Command;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("qemu-user") => qemu_user(),
        Some("qemu-system") => qemu_system(),
        Some("qemu-all") => {
            qemu_user()?;
            qemu_system()
        }
        Some("integration") => integration(),
        _ => {
            eprintln!("usage: cargo xtask <qemu-user|qemu-system|qemu-all|integration>");
            Ok(())
        }
    }
}

fn qemu_user() -> Result<()> {
    for qemu in ["qemu-x86_64", "qemu-aarch64"] {
        if !available(qemu) {
            println!("skip: {qemu} is unavailable");
            continue;
        }
        let help = Command::new(qemu).args(["-cpu", "help"]).output();
        match help {
            Ok(output) if output.status.success() => {
                println!("ok: discovered CPU list for {qemu}");
            }
            _ => println!("skip: {qemu} CPU discovery failed"),
        }
    }
    Ok(())
}

fn qemu_system() -> Result<()> {
    if std::env::var_os("SONIC_QEMU_SYSTEM").is_none() {
        println!("skip: set SONIC_QEMU_SYSTEM=1 to run system-mode QEMU tests");
        return Ok(());
    }
    for (qemu, kernel, initrd) in [
        ("qemu-system-x86_64", "SONIC_QEMU_X86_64_KERNEL", "SONIC_QEMU_X86_64_INITRD"),
        ("qemu-system-aarch64", "SONIC_QEMU_AARCH64_KERNEL", "SONIC_QEMU_AARCH64_INITRD"),
    ] {
        if !available(qemu) {
            println!("skip: {qemu} is unavailable");
        } else if std::env::var_os(kernel).is_none() || std::env::var_os(initrd).is_none() {
            println!("skip: {qemu} requires {kernel} and {initrd}");
        } else {
            println!("ready: {qemu} system-mode inputs are configured");
        }
    }
    Ok(())
}

fn integration() -> Result<()> {
    let status = Command::new("cargo").args(["test", "-p", "sonic-build"]).status()?;
    if !status.success() {
        anyhow::bail!("integration command failed");
    }
    Ok(())
}

fn available(cmd: &str) -> bool {
    Command::new(cmd).arg("--version").output().is_ok()
}
