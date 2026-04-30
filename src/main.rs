use anyhow::Result;
use clap::{Parser, Subcommand};

mod device;
mod flash;
mod tui;
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
    Flash(flash::FlashArgs),
    /// Interactive TUI for selecting an image and flashing
    Tui,
    /// Low-level rockusb operations (reused from rockusb-cli)
    Usb(usb::UsbArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Top::Flash(args) => flash::run(args),
        Top::Tui => tui::run(),
        Top::Usb(args) => usb::run(args),
    }
}
