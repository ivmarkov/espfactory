use core::cell::RefCell;

use std::fs;
use std::io::Write;

use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec::Vec;

use anyhow::Context;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

use espflash::connection::reset::{ResetAfterOperation, ResetBeforeOperation};
use espflash::elf::{ElfFirmwareImage, RomSegment};
use espflash::flasher::{FlashSettings, FlashSize, Flasher, ProgressCallbacks};
use espflash::image_format::IdfBootloaderFormat;
use espflash::targets::XtalFrequency;

use log::{debug, error};

use serialport::{FlowControl, SerialPortInfo, SerialPortType, UsbPortInfo};

use crate::bundle::{Chip, FlashData};

extern crate alloc;

/// Return the default bootloader image for the given chip
///
/// Arguments:
/// - `chip` - the chip for which the bootloader image is needed
/// - `flash_size` - the flash size to be used for the bootloader image
///   Prior to being returned, the default bootloader image is patched for the given flash size
///   (bootloader needs to know the flash size as it does some sanity checks on the app partition
///   before booting it, including whether it fits in the flash)
pub fn default_bootloader(chip: Chip, flash_size: Option<FlashSize>) -> anyhow::Result<Vec<u8>> {
    let elf_data: &[u8] = &[];

    let image = ElfFirmwareImage::try_from(elf_data)?;

    let image = bootloader_format(&image, chip, flash_size)?;

    let mut file = Vec::new();

    // There should always be a bootloader segment and it is always the first one
    // TODO: Internal `espflash` detail, maybe ask them to expose this in a more user-friendly way
    file.write_all(&image.flash_segments().next().unwrap().data)?;

    Ok(file)
}

/// Convert an ELF file to a binary image
///
/// Arguments:
/// - `elf_data` - the ELF file data
/// - `chip` - the chip for which the binary image is needed
pub fn elf2bin(elf_data: &[u8], chip: Chip) -> anyhow::Result<Vec<u8>> {
    let image = ElfFirmwareImage::try_from(elf_data)?;

    let image = bootloader_format(&image, chip, None)?;

    let mut file = Vec::new();

    for segment in image.ota_segments() {
        if file.is_empty() {
            file.write_all(&segment.data)?;
        } else {
            unreachable!("Found multiple segments in an App image");
        }
    }

    Ok(file)
}

pub async fn flash<P>(
    port: &str,
    chip: Chip,
    speed: Option<u32>,
    flash_size: Option<FlashSize>,
    flash_data: Vec<FlashData>,
    mut progress: P,
) -> anyhow::Result<()>
where
    P: ProgressCallbacks + Send + Sync + 'static,
{
    let finished = Arc::new(Signal::<CriticalSectionRawMutex, ()>::new());

    let handle = {
        let port = port.to_string();
        let finished = finished.clone();

        std::thread::spawn(move || {
            let result = (|| {
                let mut flasher = new(&port, chip, speed)?;

                if let Some(flash_size) = flash_size {
                    flasher.set_flash_size(flash_size);
                }

                let segments = flash_data
                    .iter()
                    .map(|data| RomSegment {
                        addr: data.offset,
                        data: Cow::Borrowed(data.data.as_ref()),
                    })
                    .collect::<Vec<_>>();

                flasher
                    .write_bins_to_flash(&segments, Some(&mut progress))
                    .context("Flashing failed")?;

                Ok::<_, anyhow::Error>(())
            })();

            finished.signal(());

            result?;

            Ok(())
        })
    };

    let handle = RefCell::new(Some(handle));

    let _guard = scopeguard::guard(&handle, |handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            match handle.join().unwrap() {
                Ok(()) => {}
                Err(err) => {
                    error!("Flashing returned an error: {err}");
                }
            }
        }
    });

    finished.wait().await;

    // There should be a thread handle as the guard had not kicked in yet at this point
    let handle = handle.borrow_mut().take().unwrap();

    handle.join().unwrap()
}

fn new(port: &str, chip: Chip, speed: Option<u32>) -> anyhow::Result<Flasher> {
    let port_info = get_serial_port_info(port)?;

    let serial_port = serialport::new(port_info.port_name, 112500)
        .flow_control(FlowControl::None)
        .open_native()
        .context("Opening serial port failed")?;

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
        port_info.clone(),
        speed,
        true,
        true,
        false,
        Some(chip.to_flash_chip()),
        ResetAfterOperation::default(),
        ResetBeforeOperation::default(),
    )
    .with_context(|| format!("Connecting to serial port {port_info:?} failed"))?;

    Ok(flasher)
}

fn bootloader_format<'a>(
    image: &'a ElfFirmwareImage,
    chip: Chip,
    flash_size: Option<FlashSize>,
) -> anyhow::Result<IdfBootloaderFormat<'a>> {
    let chip = chip.to_flash_chip();

    let mut flash_settings = FlashSettings::default();
    if let Some(flash_size) = flash_size {
        flash_settings.size = Some(flash_size);
    }

    let flash_data = espflash::flasher::FlashData::new(None, None, None, None, flash_settings, 0)?;

    // To get a chip revision, the connection is needed
    // For simplicity, the revision None is used
    let image = chip.into_target().get_flash_image(
        image,
        flash_data.clone(),
        None,
        XtalFrequency::default(chip),
    )?;

    Ok(image)
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
    let name = fs::canonicalize(name).with_context(|| format!("Port {name} not found"))?;
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
