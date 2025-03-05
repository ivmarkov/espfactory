//! Serial monitor utility

use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use log::{debug, error};
//#[cfg(feature = "serialport")]
use serialport::{FlowControl, SerialPort};

use espflash::cli::monitor::parser::{serial::Serial, InputParser, ResolvingPrinter};
use espflash::cli::monitor::LogFormat;

use crate::flash::get_serial_port_info;

/// Open a serial monitor on the given serial port.
pub fn monitor<W>(
    port: Option<&str>,
    elf: Option<&[u8]>,
    baud: u32,
    log_format: LogFormat,
    raw: bool,
    stop: Arc<AtomicBool>,
    out: W,
) -> anyhow::Result<()>
where
    W: std::io::Write,
{
    debug!("Opening serial monitor with baudrate: {}", baud);

    let port_info = get_serial_port_info(port)?;

    let mut serial = serialport::new(port_info.port_name, baud)
        .flow_control(FlowControl::None)
        .open_native()
        .context("Opening serial port failed")?;

    // Explicitly set the baud rate when starting the serial monitor, to allow using
    // different rates for flashing.
    serial.set_baud_rate(baud)?;
    serial.set_timeout(Duration::from_millis(5))?;

    // We are in raw mode until `_raw_mode` is dropped (ie. this function returns).
    let _raw_mode = RawModeGuard::new(raw)?;

    let mut out = ResolvingPrinter::new(elf, out);

    let mut parser: Box<dyn InputParser> = match log_format {
        LogFormat::Defmt => panic!("Defmt not supported yet"),
        _ => Box::new(Serial),
    };

    // let mut external_processors =
    //     ExternalProcessors::new(monitor_args.processors, monitor_args.elf)?;

    let mut buf = [0; 1024];

    while !stop.load(Ordering::SeqCst) {
        let read_count = match serial.read(&mut buf) {
            Ok(count) => Ok(count),
            Err(e) if e.kind() == ErrorKind::TimedOut => Ok(0),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            err => err,
        }?;

        parser.feed(&buf[0..read_count], &mut out);

        // Don't forget to flush the writer!
        out.flush().ok();
    }

    Ok(())
}

/// Type that ensures that raw mode is disabled when dropped.
struct RawModeGuard(bool);

impl RawModeGuard {
    pub fn new(raw: bool) -> anyhow::Result<Self> {
        if raw {
            enable_raw_mode()?;
        }

        Ok(Self(raw))
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.0 {
            if let Err(e) = disable_raw_mode() {
                error!("Failed to disable raw_mode: {:#}", e)
            }
        }
    }
}
