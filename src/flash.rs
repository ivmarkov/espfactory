use std::fs;
use std::io::Write;
use std::process::Command;

use alloc::borrow::Cow;
use alloc::vec::Vec;

use anyhow::Context;

use espflash::connection::reset::{ResetAfterOperation, ResetBeforeOperation};
use espflash::elf::{ElfFirmwareImage, RomSegment};
use espflash::flasher::{FlashSettings, FlashSize, Flasher, ProgressCallbacks};
use espflash::image_format::IdfBootloaderFormat;
use espflash::targets::XtalFrequency;

use log::{info, warn};

use serialport::{FlowControl, SerialPortInfo, SerialPortType, UsbPortInfo};
use tempfile::NamedTempFile;

use crate::bundle::{Chip, FlashData};

extern crate alloc;

pub(crate) const DEFAULT_BAUD_RATE: u32 = 112500;

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
    file.write_all(&image.flash_segments().next().unwrap().data)
        .context("Loading default bootloader failed")?;

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

/// Flash a binary image to the device
///
/// Arguments:
/// - `port` - the serial port to use for flashing. If not provided, the first available port where an ESP chip is detected will be used
/// - `chip` - the chip which is expected to be flashed. Used for double-checking
/// - `speed` - the baud rate to use for flashing. If not provided, the default baud rate (115_200) will be used
/// - `flash_size` - the flash size to be used for flashing. If not provided, the default flash size (4MB) will be used
/// - `flash_data` - the binary image data to be flashed
/// - `progress` - the progress callbacks to be used during flashing
#[allow(clippy::too_many_arguments)]
pub fn flash<P>(
    port: Option<&str>,
    chip: Chip,
    use_stub: bool,
    speed: Option<u32>,
    flash_size: Option<FlashSize>,
    flash_data: Vec<FlashData>,
    dry_run: bool,
    progress: &mut P,
) -> anyhow::Result<()>
where
    P: ProgressCallbacks + Send + Sync + 'static,
{
    let mut flasher = new(port, chip, use_stub, speed)?;

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

    if !dry_run {
        flasher
            .write_bins_to_flash(&segments, Some(progress))
            .context("Flashing failed")?;
    } else {
        warn!("Flash dry run mode: flashing skipped");
    }

    Ok(())
}

pub fn run_app_esptool(
    port: Option<&str>,
    chip: Chip,
    use_stub: bool,
    speed: Option<u32>,
) -> anyhow::Result<()> {
    let mut command = Command::new(esptools::Tool::EspTool.mount()?.path());

    command.arg("--chip").arg(chip.as_tools_str());

    if !use_stub {
        command.arg("--no-stub");
    }

    if let Some(port) = port {
        command.arg("--port").arg(port);
    }

    if let Some(speed) = speed {
        command.arg("--baud").arg(speed.to_string());
    }

    command.arg("run");

    let output = command
        .output()
        .with_context(|| format!("Executing `esptool.py` with command `{command:?}` failed"))?;

    if !output.status.success() {
        anyhow::bail!(
            "`{command:?}` command failed with status: {}.\nStderr output:\n{}",
            output.status,
            core::str::from_utf8(&output.stderr).unwrap_or("???")
        );
    }

    info!("`esptool.py` command `{command:?}` executed.");

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn erase(
    port: Option<&str>,
    chip: Chip,
    use_stub: bool,
    speed: Option<u32>,
    flash_size: Option<FlashSize>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut flasher = new(port, chip, use_stub, speed)?;

    if let Some(flash_size) = flash_size {
        flasher.set_flash_size(flash_size);
    }

    if !dry_run {
        flasher.erase_flash().context("Erasing flash failed")?;
    } else {
        warn!("Flash dry run mode: erasing flash skipped");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn flash_esptool<P>(
    port: Option<&str>,
    chip: Chip,
    use_stub: bool,
    speed: Option<u32>,
    flash_size: Option<FlashSize>,
    flash_data: Vec<FlashData>,
    dry_run: bool,
    progress: &mut P,
) -> anyhow::Result<()>
where
    P: ProgressCallbacks + Send + Sync + 'static,
{
    for flash_data in &flash_data {
        let mut data_temp_file =
            NamedTempFile::new().context("Creating a temporary file failed")?;

        data_temp_file
            .write_all(&flash_data.data)
            .context("Writing the binary image to a temporary file failed")?;

        data_temp_file
            .flush()
            .context("Flushing the temporary file failed")?;

        progress.init(flash_data.offset, flash_data.data.len());

        let mut command = Command::new(esptools::Tool::EspTool.mount()?.path());

        command.arg("--chip").arg(chip.as_tools_str());

        if !use_stub {
            command.arg("--no-stub");
        }

        if let Some(port) = port {
            command.arg("--port").arg(port);
        }

        if let Some(speed) = speed {
            command.arg("--baud").arg(speed.to_string());
        }

        command.arg("--after").arg("no_reset");

        command
            .arg("write_flash")
            .arg(format!("0x{:x}", flash_data.offset))
            .arg(data_temp_file.path());

        if let Some(flash_size) = flash_size {
            command.arg("--flash_size").arg(format!("{flash_size}"));
        }

        // Necessary for chips in Secure Download Mode
        command.arg("--force");

        if !dry_run {
            warn!("About to execute `esptool.py` command `{command:?}`...");

            let output = command.output().with_context(|| {
                format!("Executing `esptool.py` with command `{command:?}` failed")
            })?;

            if !output.status.success() {
                anyhow::bail!(
                    "`{command:?}` command failed with status: {}.\nStderr output:\n{}",
                    output.status,
                    core::str::from_utf8(&output.stderr).unwrap_or("???")
                );
            }

            info!("`esptool.py` command `{command:?}` executed.");
        } else {
            warn!("Flash dry run mode: flashing skipped");
        }

        progress.finish();
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn erase_esptool(
    port: Option<&str>,
    chip: Chip,
    use_stub: bool,
    speed: Option<u32>,
    _flash_size: Option<FlashSize>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut command = Command::new(esptools::Tool::EspTool.mount()?.path());

    command.arg("--chip").arg(chip.as_tools_str());

    if !use_stub {
        command.arg("--no-stub");
    }

    if let Some(port) = port {
        command.arg("--port").arg(port);
    }

    if let Some(speed) = speed {
        command.arg("--baud").arg(speed.to_string());
    }

    command.arg("--after").arg("no_reset");

    command.arg("erase_flash");

    // Necessary for chips in Secure Download Mode
    command.arg("--force");

    if !dry_run {
        warn!("About to execute `esptool.py` command `{command:?}`...");

        let output = command
            .output()
            .with_context(|| format!("Executing `esptool.py` with command `{command:?}` failed"))?;

        if !output.status.success() {
            anyhow::bail!(
                "`{command:?}` command failed with status: {}.\nStderr output:\n{}Stdout output:\n{}",
                output.status,
                core::str::from_utf8(&output.stderr).unwrap_or("???"),
                core::str::from_utf8(&output.stdout).unwrap_or("???")
            );
        }

        info!("`esptool.py` command `{command:?}` executed.");
    } else {
        warn!("Flash dry run mode: erasing flash skipped");
    }

    Ok(())
}

pub fn encrypt(offset: usize, raw_data: &[u8], key: &[u8]) -> anyhow::Result<Vec<u8>> {
    let key_file = NamedTempFile::new().context("Creating temp key file failed")?;
    fs::write(key_file.path(), key).context("Creating temp key file failed")?;

    let espsecure = esptools::Tool::EspSecure.mount()?;

    let input_file = NamedTempFile::new().context("Creating temp input file failed")?;
    fs::write(input_file.path(), raw_data).context("Creating temp input file failed")?;

    let output_file = NamedTempFile::new().context("Creating temp output file failed")?;

    let mut command = Command::new(espsecure.path());

    command
        .arg("encrypt_flash_data")
        .arg("--aes_xts")
        .arg("--keyfile")
        .arg(key_file.path())
        .arg("--address")
        .arg(format!("0x{:x}", offset))
        .arg("--output")
        .arg(output_file.path())
        .arg(input_file.path());

    let output = command.output().with_context(|| {
        "Executing the espsecure tool with command `encrypt_flash_data` failed".to_string()
    })?;

    if !output.status.success() {
        anyhow::bail!(
            "espsecure tool `encrypt_flash_data` command failed with status: {}. Stderr output:\n{}",
            output.status,
            core::str::from_utf8(&output.stderr).unwrap_or("???")
        );
    }

    let data = fs::read(output_file.path()).context("Reading encrypted data failed")?;

    Ok(data)
}

pub fn empty_space(size: usize) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(size);
    chunk.resize(size, 0xff);

    chunk
}

fn new(
    port: Option<&str>,
    chip: Chip,
    use_stub: bool,
    speed: Option<u32>,
) -> anyhow::Result<Flasher> {
    let port_info = get_serial_port_info(port)?;

    let serial_port = serialport::new(port_info.port_name, DEFAULT_BAUD_RATE)
        .flow_control(FlowControl::None)
        .open_native()
        .context("Opening serial port failed")?;

    // NOTE: since `get_serial_port_info` filters out all PCI Port and Bluetooth
    //       serial ports, we can just pretend these types don't exist here.
    let port_info = match port_info.port_type {
        SerialPortType::UsbPort(info) => info,
        SerialPortType::PciPort | SerialPortType::Unknown => {
            warn!("Matched `SerialPortType::PciPort or ::Unknown`");
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
        use_stub,
        true,
        false,
        Some(chip.to_flash_chip()),
        ResetAfterOperation::NoReset,
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
pub(crate) fn get_serial_port_info(serial: Option<&str>) -> anyhow::Result<SerialPortInfo> {
    let ports = detect_usb_serial_ports(false).unwrap_or_default();
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
fn find_serial_port(
    ports: &[SerialPortInfo],
    name: Option<&str>,
) -> anyhow::Result<SerialPortInfo> {
    if let Some(name) = name {
        info!("Finding serial port {name}");

        #[cfg(not(target_os = "windows"))]
        let name = std::fs::canonicalize(name).with_context(|| format!("Port {name} not found"))?;
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
            info!("Serial port {name} found");

            Ok(port.to_owned())
        } else {
            anyhow::bail!("Serial port not found: {}", name)
        }
    } else {
        info!("Detecting serial port...");

        if ports.is_empty() {
            anyhow::bail!("No serial ports found")
        }

        info!(
            "Using the first available serial port `{}` from [{}]",
            ports[0].port_name,
            ports
                .iter()
                .map(|port| port.port_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );

        Ok(ports[0].to_owned())
    }
}
