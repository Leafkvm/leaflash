use anyhow::Result;
use clap::{Parser, Subcommand};

mod device;
mod flash;
mod mtdparts;
mod tui;
mod uboot;
mod uboot_env;
mod usb;

#[derive(Parser)]
#[command(name = "leaflash", version, about = "Development CLI for LeafKVM")]
struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(Subcommand)]
enum Top {
    /// Flash an image to the device's SD card with an A/B/userdata layout
    #[command(alias = "f", alias = "sd")]
    Flash(flash::FlashArgs),
    /// Write a u-boot / SPI-NOR image to the device's SPI NOR
    #[command(alias = "u", alias = "spi")]
    Uboot(uboot::UbootArgs),
    /// Interactive TUI for selecting an image and flashing
    #[command(alias = "t")]
    Tui,
    /// Low-level rockusb operations (reused from rockusb-cli)
    #[command(alias = "rk")]
    Usb(usb::UsbArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Top::Flash(args) => flash::run(args),
        Top::Uboot(args) => uboot::run(args),
        Top::Tui => tui::run(),
        Top::Usb(args) => usb::run(args),
    }
}
