use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Context;

use serde::{Deserialize, Serialize};

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
    chip: Option<&str>,
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
        command.arg("--chip").arg(chip);
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
        .context("Executing the eFuse tool with command `summary` failed")?;

    if !status.success() {
        anyhow::bail!(
            "eFuse tool `summary` command failed with status: {}. Is the PCB connected?",
            status
        );
    }

    let summary = fs::read_to_string(tempfile.path())
        .context("Reading the eFuse tool `summary` command output failed")?;

    let summary = serde_json::from_str::<HashMap<String, EfuseValue>>(&summary)
        .context("Parsing the eFuse tool `summary` command output failed")?;

    Ok(summary)
}

pub fn burn_efuses<'a, I>(
    chip: Option<&str>,
    port: Option<&str>,
    baud: Option<&str>,
    dry_run: bool,
    values: I,
) -> anyhow::Result<String>
where
    I: Iterator<Item = (&'a str, u32)>,
{
    let mut command = Command::new(esptools::Tool::EspEfuse.mount()?.path());

    if let Some(chip) = chip {
        command.arg("--chip").arg(chip);
    }

    if let Some(port) = port {
        command.arg("--port").arg(port);
    }

    if let Some(baud) = baud {
        command.arg("--baud").arg(baud);
    }

    command.arg("burn_efuse");

    for (key, value) in values {
        command.arg(key);
        command.arg(value.to_string());
    }

    burn_exec("burn_efuse", dry_run, &mut command)
}

pub fn burn_keys<'a, I>(
    chip: Option<&str>,
    port: Option<&str>,
    baud: Option<&str>,
    dry_run: bool,
    values: I,
) -> anyhow::Result<String>
where
    I: Iterator<Item = (&'a str, &'a [u8], &'a str)>,
{
    burn_keys_or_digests("burn_key", chip, port, baud, dry_run, values)
}

pub fn burn_key_digests<'a, I>(
    chip: Option<&str>,
    port: Option<&str>,
    baud: Option<&str>,
    dry_run: bool,
    values: I,
) -> anyhow::Result<String>
where
    I: Iterator<Item = (&'a str, &'a [u8], &'a str)>,
{
    burn_keys_or_digests("burn_key_digest", chip, port, baud, dry_run, values)
}

fn burn_keys_or_digests<'a, I>(
    cmd: &str,
    chip: Option<&str>,
    port: Option<&str>,
    baud: Option<&str>,
    dry_run: bool,
    values: I,
) -> anyhow::Result<String>
where
    I: Iterator<Item = (&'a str, &'a [u8], &'a str)>,
{
    let mut command = Command::new(esptools::Tool::EspEfuse.mount()?.path());

    if let Some(chip) = chip {
        command.arg("--chip").arg(chip);
    }

    if let Some(port) = port {
        command.arg("--port").arg(port);
    }

    if let Some(baud) = baud {
        command.arg("--baud").arg(baud);
    }

    command.arg(cmd);

    let mut temp_files = Vec::new();

    for (key, value, purpose) in values {
        command.arg(key);

        let mut temp_file =
            tempfile::NamedTempFile::new().context("Creation of eFuse temp key file failed")?;

        temp_file
            .write_all(value)
            .context("Writing eFuse temp key file failed")?;

        command.arg(temp_file.path().to_string_lossy().into_owned());

        temp_files.push(temp_file);

        command.arg(purpose);
    }

    burn_exec(cmd, dry_run, &mut command)
}

fn burn_exec(command_desc: &str, dry_run: bool, command: &mut Command) -> anyhow::Result<String> {
    if dry_run {
        return Ok("".to_string());
    }

    let output = command
        .output()
        .context("Executing the eFuse tool with command `{command_desc}` failed")?;

    if !output.status.success() {
        anyhow::bail!(
            "eFuse tool `{command_desc}` command failed with status: {}. Is the PCB connected? Stderr output:\n{}",
            output.status,
            core::str::from_utf8(&output.stderr).unwrap_or("???")
        );
    }

    core::str::from_utf8(&output.stdout)
        .context("Parsing the eFuse tool `{command_desc}` command output failed")
        .map(str::to_string)
}
