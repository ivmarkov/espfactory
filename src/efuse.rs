use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Context;

use log::{info, warn};

use serde::{Deserialize, Serialize};

use crate::bundle::Chip;

/// An eFuse value as returned by the Espressif eFuse tool when the command `espefuse summary --format json` is used
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EfuseValue {
    pub bit_len: u16,
    pub block: u8,
    pub category: String,
    pub description: String,
    pub efuse_type: String,
    pub name: String,
    pub pos: Option<u16>,
    pub readable: bool,
    pub value: serde_json::Value,
    pub word: Option<u16>,
    pub writeable: bool,
}

/// Get the eFuse summary for the given values
///
/// # Arguments
/// - `tools`: The tools to use for the eFuse tool
/// - `values`: The eFuse values to get the summary for. If empty, all values are returned
///
/// # Returns
/// A map of eFuse values by name
pub fn summary<'a, I>(
    chip: Option<Chip>,
    port: Option<&str>,
    baud: Option<&str>,
    values: I,
) -> anyhow::Result<HashMap<String, EfuseValue>>
where
    I: Iterator<Item = &'a str>,
{
    let tempfile =
        tempfile::NamedTempFile::new().context("Creation of eFuse temp out file failed")?;

    let mut command = Command::new(esptools::Tool::EspEfuse.mount()?.path());

    if let Some(chip) = chip {
        command.arg("--chip").arg(chip.as_tools_str());
    }

    if let Some(port) = port {
        command.arg("--port").arg(port);
    }

    if let Some(baud) = baud {
        command.arg("--baud").arg(baud);
    }

    command
        .arg("summary")
        .arg("--format")
        .arg("json")
        .arg("--file")
        .arg(tempfile.path().to_string_lossy().into_owned());

    for value in values {
        command.arg(value);
    }

    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());

    let status = command
        .status()
        .context("Executing the eFuse tool with command `{command:?}` failed")?;

    if !status.success() {
        anyhow::bail!(
            "eFuse tool command {command:?} failed with status: {}. Is the PCB connected?",
            status
        );
    }

    let summary = fs::read_to_string(tempfile.path()).with_context(|| {
        format!("Reading the eFuse tool command `{command:?}` command output failed")
    })?;

    let summary = serde_json::from_str::<HashMap<String, EfuseValue>>(&summary)
        .with_context(|| format!("Parsing the eFuse tool command `{command:?}` command output===\n{summary}\n=== failed"))?;

    Ok(summary)
}

pub fn burn_efuses<'a, I>(
    chip: Chip,
    port: Option<&str>,
    baud: Option<&str>,
    dry_run: bool,
    values: I,
) -> anyhow::Result<String>
where
    I: Iterator<Item = (&'a str, u32)>,
{
    let mut command = Command::new(esptools::Tool::EspEfuse.mount()?.path());

    command.arg("--chip").arg(chip.as_tools_str());

    if let Some(port) = port {
        command.arg("--port").arg(port);
    }

    if let Some(baud) = baud {
        command.arg("--baud").arg(baud);
    }

    command.arg("--do-not-confirm");

    command.arg("burn_efuse");

    for (key, value) in values {
        command.arg(key);
        command.arg(value.to_string());
    }

    burn_exec(dry_run, &mut command)
}

pub fn burn_keys<'a, I>(
    protect_keys: bool,
    chip: Chip,
    port: Option<&str>,
    baud: Option<&str>,
    dry_run: bool,
    values: I,
) -> anyhow::Result<String>
where
    I: Iterator<Item = (&'a str, &'a [u8], &'a str)>,
{
    burn_keys_or_digests(protect_keys, "burn_key", chip, port, baud, dry_run, values)
}

pub fn burn_key_digests<'a, I>(
    protect_digests: bool,
    chip: Chip,
    port: Option<&str>,
    baud: Option<&str>,
    dry_run: bool,
    values: I,
) -> anyhow::Result<String>
where
    I: Iterator<Item = (&'a str, &'a [u8], &'a str)>,
{
    burn_keys_or_digests(
        protect_digests,
        "burn_key_digest",
        chip,
        port,
        baud,
        dry_run,
        values,
    )
}

fn burn_keys_or_digests<'a, I>(
    protect_keys: bool,
    cmd: &str,
    chip: Chip,
    port: Option<&str>,
    baud: Option<&str>,
    dry_run: bool,
    values: I,
) -> anyhow::Result<String>
where
    I: Iterator<Item = (&'a str, &'a [u8], &'a str)>,
{
    let mut command = Command::new(esptools::Tool::EspEfuse.mount()?.path());

    command.arg("--chip").arg(chip.as_tools_str());

    if let Some(port) = port {
        command.arg("--port").arg(port);
    }

    if let Some(baud) = baud {
        command.arg("--baud").arg(baud);
    }

    // ... or else we need to type "BURN" in the terminal which is impossible
    // as the provisioning process is not interactive
    command.arg("--do-not-confirm");

    command.arg(cmd);

    // NOTE: VERY, VERY IMPORTANT
    // As mentoned here:
    // https://docs.espressif.com/projects/esp-idf/en/v5.4/esp32s3/security/security-features-enablement-workflows.html#enable-flash-encryption-and-secure-boot-v2-externally
    // ... all keys and digests are actually protected by using two bit-fields in the eFuse block 0:
    // WR_DIS (BLOCK0)                                    Disable programming of individual eFuses           = 25166593 R/W (0x01800301)
    // RD_DIS (BLOCK0)                                    Disable reading from BlOCK4-10                     = 0 R/- (0b0000000)
    //
    // So by burning specifically a Secure Boot V2 digest first (which needs to be readable but NOT writable), we need burn
    // a few its in `WR_DIS` and `RD_DIS` to write-protect it and (where the logic breaks) to make sure
    // it remains readable.
    // The last one (ensuring the key remains readable) is - unfortunately - implemented by write-protecting the RD_DIS
    // bit-field **itself** (so that a hacker cannot read-protected the Secfure Boot signature, thus causing denial of service).
    //
    // Unfortunately, this means that we cannot read-protect a subsequent Flash Encryption key burn, as we cannot flip the
    // corresponding bit in RD_DIS, as the RD_DIS bitfield itself is now write-protected.
    //
    // Therefore, the workaround here is just making sure we don't do anything with the RD_DIS and WR_DIS fields.
    //
    // The bootloader would fix these anyway, when configured properly.
    //
    // See also:
    // https://github.com/espressif/esp-idf/issues/11888
    if !protect_keys {
        if matches!(chip, Chip::Esp32) {
            command.arg("--no-protect-key");
        } else {
            command.arg("--no-read-protect").arg("--no-write-protect");
        }
    }

    let mut temp_files = Vec::new();

    for (key, value, purpose) in values {
        command.arg(key);

        let mut temp_file = tempfile::NamedTempFile::new()
            .context("Creation of eFuse temp key/digest file failed")?;

        temp_file
            .write_all(value)
            .context("Writing eFuse temp key/digest file failed")?;

        temp_file
            .flush()
            .context("Flushing eFuse temp key/digest file failed")?;

        command.arg(temp_file.path().to_string_lossy().into_owned());

        temp_files.push(temp_file);

        command.arg(purpose);
    }

    burn_exec(dry_run, &mut command)
}

fn burn_exec(dry_run: bool, command: &mut Command) -> anyhow::Result<String> {
    if dry_run {
        warn!("eFuse dry run mode: NOT executing eFuse tool command `{command:?}`");
        return Ok("".to_string());
    }

    warn!("About to execute eFuse tool command `{command:?}`...");

    let output = command
        .output()
        .with_context(|| format!("Executing the eFuse tool with command `{command:?}` failed"))?;

    if !output.status.success() {
        anyhow::bail!(
            "eFuse tool `{command:?}` command failed with status: {}. Is the PCB connected?\nStderr output:\n{}\nStdout output:\n{}",
            output.status,
            core::str::from_utf8(&output.stderr).unwrap_or("???"),
            core::str::from_utf8(&output.stdout).unwrap_or("???")
        );
    }

    let output = core::str::from_utf8(&output.stdout)
        .with_context(|| format!("Loading the eFuse tool `{command:?}` command output failed"))
        .map(str::to_string)?;

    info!("eFuse tool command `{command:?}` executed. Output:\n{output}");

    Ok(output)
}
