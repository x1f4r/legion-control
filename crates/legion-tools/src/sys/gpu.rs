//! NVIDIA GPUs, as `nvidia-smi` reports them.
//!
//! A machine without the tool, or whose driver does not answer in time, has no GPUs as far as
//! `sys.info` is concerned: a missing GPU is an answer, not a failure of the call.

use std::time::Duration;

use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use serde::Serialize;

/// One GPU in `sys.info`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Gpu {
    /// The model name.
    pub name: String,
    /// Its memory, in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_total: Option<u64>,
    /// Memory in use, in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used: Option<u64>,
    /// How busy it is, in percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization_pct: Option<f64>,
}

/// How long `nvidia-smi` may take. It can be slow to wake a GPU from power saving, but `sys.info`
/// must answer within a couple of seconds.
const TIMEOUT: Duration = Duration::from_millis(1500);

/// Asks `nvidia-smi`, returning no GPUs when it is absent, fails or is too slow.
pub(super) async fn query() -> Vec<Gpu> {
    let command = CommandSpec::argv(
        "nvidia-smi",
        [
            "--query-gpu=name,memory.total,memory.used,utilization.gpu",
            "--format=csv,noheader,nounits",
        ],
    );
    let options = RunOptions::new(command)
        .c_locale()
        .timeout(TIMEOUT)
        .kill_grace(Duration::from_millis(200))
        .capture_limit(64 * 1024);
    match run(options).await {
        Ok(output) if output.success() => parse(&output.stdout.text),
        _ => Vec::new(),
    }
}

/// One GPU per line, `name, memory.total, memory.used, utilization.gpu`, memory in MiB. A value
/// the driver does not know is `[N/A]` or similar, which becomes `null`.
fn parse(text: &str) -> Vec<Gpu> {
    const MIB: u64 = 1024 * 1024;
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            let [name, total, used, utilization] = fields.as_slice() else {
                return None;
            };
            if name.is_empty() {
                return None;
            }
            Some(Gpu {
                name: (*name).to_owned(),
                memory_total: total.parse::<u64>().ok().map(|mib| mib * MIB),
                memory_used: used.parse::<u64>().ok().map(|mib| mib * MIB),
                utilization_pct: utilization.parse::<f64>().ok(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_line_is_a_gpu_with_memory_in_bytes() {
        let gpus = parse(
            "NVIDIA GeForce RTX 5080, 16303, 1024, 7\nNVIDIA A2, [N/A], 0, [N/A]\n\ngarbage\n",
        );
        assert_eq!(
            gpus,
            [
                Gpu {
                    name: "NVIDIA GeForce RTX 5080".to_owned(),
                    memory_total: Some(16303 * 1024 * 1024),
                    memory_used: Some(1024 * 1024 * 1024),
                    utilization_pct: Some(7.0),
                },
                Gpu {
                    name: "NVIDIA A2".to_owned(),
                    memory_total: None,
                    memory_used: Some(0),
                    utilization_pct: None,
                },
            ]
        );
    }
}
