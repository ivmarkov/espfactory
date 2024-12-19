use std::collections::HashMap;
use std::fs;
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
    tools: &esptools::Tools,
    values: I,
) -> anyhow::Result<HashMap<String, EfuseValue>>
where
    I: Iterator<Item = &'a str>,
{
    let tempfile =
        tempfile::NamedTempFile::new().context("Creation of eFuse temp out file failed")?;

    let mut command = Command::new(tools.tool_path(esptools::Tool::EspEfuse));

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

    command
        .status()
        .context("Executing the eFuse tool with command `summary` failed")?;

    let summary = fs::read_to_string(tempfile.path())
        .context("Reading the eFuse tool `summary` command output failed")?;

    let summary = serde_json::from_str::<HashMap<String, EfuseValue>>(&summary)
        .context("Parsing the eFuse tool `summary` command output failed")?;

    Ok(summary)
}
