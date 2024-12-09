use std::borrow::Cow;
use std::fs;
use std::sync::Arc;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use espflash::connection::reset::{ResetAfterOperation, ResetBeforeOperation};
use espflash::elf::RomSegment;
use espflash::flasher::{FlashSize, Flasher, ProgressCallbacks};
use espflash::targets::Chip;

use log::debug;
use serialport::{FlowControl, SerialPortInfo, SerialPortType, UsbPortInfo};

use crate::bundle::FlashData;

pub async fn flash<P>(
    port: &str,
    flash_size: Option<FlashSize>,
    flash_data: Vec<FlashData>,
    mut progress: P,
) -> anyhow::Result<()>
where
    P: ProgressCallbacks + Send + Sync + 'static,
{
    let finished = Arc::new(Signal::<CriticalSectionRawMutex, ()>::new());

    let flasher_thread = {
        let port = port.to_string();
        let finished = finished.clone();

        std::thread::spawn(move || {
            let mut flasher = new(&port).unwrap();

            if let Some(flash_size) = flash_size {
                flasher.set_flash_size(flash_size);
            }

            let segments = flash_data
                .iter()
                .map(|data| RomSegment {
                    addr: data.offsert,
                    data: Cow::Borrowed(data.data.as_ref()),
                })
                .collect::<Vec<_>>();

            flasher
                .write_bins_to_flash(&segments, Some(&mut progress))
                .unwrap();

            finished.signal(());
        })
    };

    finished.wait().await;

    flasher_thread.join(); // TODO: Join on drop

    Ok(())
}

fn new(port: &str) -> anyhow::Result<Flasher> {
    let port_info = get_serial_port_info(port)?;

    let serial_port = serialport::new(port_info.port_name, 112500)
        .flow_control(FlowControl::None)
        .open_native()?;

    // NOTE: since `get_serial_port_info` filters out all PCI Port and Bluetooth
    //       serial ports, we can just pretend these types don't exist here.
    let port_info = match port_info.port_type {
        SerialPortType::UsbPort(info) => info,
        SerialPortType::PciPort | SerialPortType::Unknown => {
            debug!("Matched `SerialPortType::PciPort or ::Unknown`");
            UsbPortInfo {
                vid: 0,
                pid: 0,
                serial_number: None,
                manufacturer: None,
                product: None,
            }
        }
        _ => unreachable!(),
    };

    let flasher = espflash::flasher::Flasher::connect(
        *Box::new(serial_port),
        port_info,
        Some(115200), // TODO
        true,
        true,
        false,
        Some(Chip::Esp32s3), // TODO
        ResetAfterOperation::default(),
        ResetBeforeOperation::default(),
    )?;

    Ok(flasher)
}

/// Return the information of a serial port taking into account the different
/// ways of choosing a port.
fn get_serial_port_info(serial: &str) -> anyhow::Result<SerialPortInfo> {
    let ports = detect_usb_serial_ports(true).unwrap_or_default();
    find_serial_port(&ports, serial)
}

// TODO: musl
fn detect_usb_serial_ports(list_all_ports: bool) -> anyhow::Result<Vec<SerialPortInfo>> {
    let ports = serialport::available_ports()?;
    let ports = ports
        .into_iter()
        .filter(|port_info| {
            if list_all_ports {
                matches!(
                    &port_info.port_type,
                    SerialPortType::UsbPort(..) |
                    // Allow PciPort. The user may want to use it.
                    // The port might have been misdetected by the system as PCI.
                    SerialPortType::PciPort |
                    // Good luck.
                    SerialPortType::Unknown
                )
            } else {
                matches!(&port_info.port_type, SerialPortType::UsbPort(..))
            }
        })
        .collect::<Vec<_>>();

    Ok(ports)
}

/// Given a vector of `SerialPortInfo` structs, attempt to find and return one
/// whose `port_name` field matches the provided `name` argument.
fn find_serial_port(ports: &[SerialPortInfo], name: &str) -> anyhow::Result<SerialPortInfo> {
    #[cfg(not(target_os = "windows"))]
    let name = fs::canonicalize(name)?;
    #[cfg(not(target_os = "windows"))]
    let name = name.to_string_lossy();

    // The case in device paths matters in BSD!
    #[cfg(any(
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    let port_info = ports.iter().find(|port| port.port_name == name);

    // On Windows and other *nix systems, the case is not important.
    #[cfg(not(any(
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    let port_info = ports
        .iter()
        .find(|port| port.port_name.eq_ignore_ascii_case(name.as_ref()));

    if let Some(port) = port_info {
        Ok(port.to_owned())
    } else {
        anyhow::bail!("Serial port not found: {}", name)
    }
}
