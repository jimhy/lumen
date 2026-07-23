use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub(crate) const LINUX_METRICS_COMMAND: &str = r#"LC_ALL=C; printf 'LUMEN_METRICS_V1\n'; awk '/^cpu / {print "cpu=" $2 "," $3 "," $4 "," $5 "," $6 "," $7 "," $8 "," $9; exit}' /proc/stat; awk '{print "load=" $1 "," $2 "," $3; exit}' /proc/loadavg; awk '/^MemTotal:/ {total=$2} /^MemAvailable:/ {available=$2} END {if (total != "") print "mem_kib=" total "," available}' /proc/meminfo; df -PkP / 2>/dev/null | awk 'NR == 2 {print "disk_kib=" $2 "," $4}'; awk 'NR > 2 {line=$0; sub(/^[[:space:]]+/, "", line); count=split(line, value, /[:[:space:]]+/); if (count >= 10 && value[1] != "lo") {rx += value[2]; tx += value[10]}} END {print "net=" rx+0 "," tx+0}' /proc/net/dev; awk '{print "uptime=" $1; exit}' /proc/uptime"#;

const MAX_METRICS_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerMetrics {
    pub cpu_usage_percent: Option<f32>,
    pub load_average_1m: f64,
    pub load_average_5m: f64,
    pub load_average_15m: f64,
    pub memory: SystemMemoryMetrics,
    pub root_disk: StorageMetrics,
    pub network: NetworkMetrics,
    pub uptime_seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemMemoryMetrics {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageMetrics {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetworkMetrics {
    pub received_bytes: u64,
    pub transmitted_bytes: u64,
    pub receive_bytes_per_second: Option<f64>,
    pub transmit_bytes_per_second: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CounterSnapshot {
    cpu_total: u64,
    cpu_idle: u64,
    received_bytes: u64,
    transmitted_bytes: u64,
}

/// Carries the previous raw counters needed for CPU and network deltas.
#[derive(Clone, Debug, Default)]
pub struct MetricsAccumulator {
    previous: Option<CounterSnapshot>,
}

impl MetricsAccumulator {
    #[must_use]
    pub const fn new() -> Self {
        Self { previous: None }
    }

    /// Parses one versioned monitor response and computes rates relative to the
    /// preceding sample.
    ///
    /// `elapsed` is ignored for the first sample. Counter resets/wraps produce
    /// `None` for the affected derived value instead of a false spike.
    pub fn update(
        &mut self,
        output: &str,
        elapsed: Option<Duration>,
    ) -> Result<ServerMetrics, MetricsError> {
        let raw = RawMetrics::parse(output)?;
        let current = CounterSnapshot {
            cpu_total: raw.cpu_total,
            cpu_idle: raw.cpu_idle,
            received_bytes: raw.received_bytes,
            transmitted_bytes: raw.transmitted_bytes,
        };

        let cpu_usage_percent = self.previous.and_then(|previous| {
            let total_delta = current.cpu_total.checked_sub(previous.cpu_total)?;
            let idle_delta = current.cpu_idle.checked_sub(previous.cpu_idle)?;
            if total_delta == 0 || idle_delta > total_delta {
                return None;
            }
            let busy_delta = total_delta - idle_delta;
            Some((busy_delta as f64 * 100.0 / total_delta as f64) as f32)
        });

        let rate_seconds = elapsed
            .map(|duration| duration.as_secs_f64())
            .filter(|value| *value > 0.0);
        let (receive_bytes_per_second, transmit_bytes_per_second) =
            match (self.previous, rate_seconds) {
                (Some(previous), Some(seconds)) => (
                    current
                        .received_bytes
                        .checked_sub(previous.received_bytes)
                        .map(|delta| delta as f64 / seconds),
                    current
                        .transmitted_bytes
                        .checked_sub(previous.transmitted_bytes)
                        .map(|delta| delta as f64 / seconds),
                ),
                _ => (None, None),
            };

        self.previous = Some(current);

        Ok(ServerMetrics {
            cpu_usage_percent,
            load_average_1m: raw.load_average[0],
            load_average_5m: raw.load_average[1],
            load_average_15m: raw.load_average[2],
            memory: SystemMemoryMetrics {
                total_bytes: kib_to_bytes(raw.memory_total_kib)?,
                used_bytes: kib_to_bytes(raw.memory_total_kib - raw.memory_available_kib)?,
                available_bytes: kib_to_bytes(raw.memory_available_kib)?,
            },
            root_disk: StorageMetrics {
                total_bytes: kib_to_bytes(raw.disk_total_kib)?,
                used_bytes: kib_to_bytes(raw.disk_total_kib - raw.disk_available_kib)?,
                available_bytes: kib_to_bytes(raw.disk_available_kib)?,
            },
            network: NetworkMetrics {
                received_bytes: raw.received_bytes,
                transmitted_bytes: raw.transmitted_bytes,
                receive_bytes_per_second,
                transmit_bytes_per_second,
            },
            uptime_seconds: raw.uptime_seconds,
        })
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MetricsError {
    #[error("monitor output is too large")]
    OutputTooLarge,
    #[error("unsupported monitor output version")]
    UnsupportedVersion,
    #[error("monitor output contains an unknown field")]
    UnknownField,
    #[error("monitor output contains a duplicate field")]
    DuplicateField,
    #[error("monitor output is missing {0}")]
    MissingField(&'static str),
    #[error("monitor output contains an invalid {0}")]
    InvalidValue(&'static str),
    #[error("monitor counter overflow")]
    CounterOverflow,
}

#[derive(Clone, Debug)]
struct RawMetrics {
    cpu_total: u64,
    cpu_idle: u64,
    load_average: [f64; 3],
    memory_total_kib: u64,
    memory_available_kib: u64,
    disk_total_kib: u64,
    disk_available_kib: u64,
    received_bytes: u64,
    transmitted_bytes: u64,
    uptime_seconds: f64,
}

impl RawMetrics {
    fn parse(output: &str) -> Result<Self, MetricsError> {
        if output.len() > MAX_METRICS_OUTPUT_BYTES {
            return Err(MetricsError::OutputTooLarge);
        }

        let mut lines = output.lines().filter(|line| !line.trim().is_empty());
        if lines.next().map(str::trim) != Some("LUMEN_METRICS_V1") {
            return Err(MetricsError::UnsupportedVersion);
        }

        let mut cpu = None;
        let mut load = None;
        let mut memory = None;
        let mut disk = None;
        let mut network = None;
        let mut uptime = None;

        for line in lines {
            let (key, value) = line
                .trim()
                .split_once('=')
                .ok_or(MetricsError::UnknownField)?;
            match key {
                "cpu" => set_once(&mut cpu, parse_cpu(value)?)?,
                "load" => set_once(&mut load, parse_float_triplet(value, "load")?)?,
                "mem_kib" => {
                    set_once(&mut memory, parse_u64_pair(value, "memory")?)?;
                }
                "disk_kib" => {
                    set_once(&mut disk, parse_u64_pair(value, "disk")?)?;
                }
                "net" => {
                    set_once(&mut network, parse_u64_pair(value, "network")?)?;
                }
                "uptime" => set_once(&mut uptime, parse_nonnegative_float(value, "uptime")?)?,
                _ => return Err(MetricsError::UnknownField),
            }
        }

        let (cpu_total, cpu_idle) = cpu.ok_or(MetricsError::MissingField("cpu"))?;
        let load_average = load.ok_or(MetricsError::MissingField("load"))?;
        let (memory_total_kib, memory_available_kib) =
            memory.ok_or(MetricsError::MissingField("memory"))?;
        let (disk_total_kib, disk_available_kib) =
            disk.ok_or(MetricsError::MissingField("disk"))?;
        let (received_bytes, transmitted_bytes) =
            network.ok_or(MetricsError::MissingField("network"))?;
        let uptime_seconds = uptime.ok_or(MetricsError::MissingField("uptime"))?;

        if memory_available_kib > memory_total_kib {
            return Err(MetricsError::InvalidValue("memory"));
        }
        if disk_available_kib > disk_total_kib {
            return Err(MetricsError::InvalidValue("disk"));
        }

        Ok(Self {
            cpu_total,
            cpu_idle,
            load_average,
            memory_total_kib,
            memory_available_kib,
            disk_total_kib,
            disk_available_kib,
            received_bytes,
            transmitted_bytes,
            uptime_seconds,
        })
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), MetricsError> {
    if slot.replace(value).is_some() {
        return Err(MetricsError::DuplicateField);
    }
    Ok(())
}

fn parse_cpu(value: &str) -> Result<(u64, u64), MetricsError> {
    let values = parse_u64_list::<8>(value, "cpu")?;
    let total = values
        .iter()
        .try_fold(0_u64, |sum, value| sum.checked_add(*value))
        .ok_or(MetricsError::CounterOverflow)?;
    let idle = values[3]
        .checked_add(values[4])
        .ok_or(MetricsError::CounterOverflow)?;
    if idle > total {
        return Err(MetricsError::InvalidValue("cpu"));
    }
    Ok((total, idle))
}

fn parse_float_triplet(value: &str, field: &'static str) -> Result<[f64; 3], MetricsError> {
    let mut parts = value.split(',');
    let result = [
        parse_nonnegative_float(
            parts.next().ok_or(MetricsError::InvalidValue(field))?,
            field,
        )?,
        parse_nonnegative_float(
            parts.next().ok_or(MetricsError::InvalidValue(field))?,
            field,
        )?,
        parse_nonnegative_float(
            parts.next().ok_or(MetricsError::InvalidValue(field))?,
            field,
        )?,
    ];
    if parts.next().is_some() {
        return Err(MetricsError::InvalidValue(field));
    }
    Ok(result)
}

fn parse_u64_pair(value: &str, field: &'static str) -> Result<(u64, u64), MetricsError> {
    let values = parse_u64_list::<2>(value, field)?;
    Ok((values[0], values[1]))
}

fn parse_u64_list<const N: usize>(
    value: &str,
    field: &'static str,
) -> Result<[u64; N], MetricsError> {
    let mut values = [0_u64; N];
    let mut parts = value.split(',');
    for slot in &mut values {
        let part = parts.next().ok_or(MetricsError::InvalidValue(field))?;
        *slot = part
            .parse::<u64>()
            .map_err(|_| MetricsError::InvalidValue(field))?;
    }
    if parts.next().is_some() {
        return Err(MetricsError::InvalidValue(field));
    }
    Ok(values)
}

fn parse_nonnegative_float(value: &str, field: &'static str) -> Result<f64, MetricsError> {
    let value = value
        .parse::<f64>()
        .map_err(|_| MetricsError::InvalidValue(field))?;
    if !value.is_finite() || value < 0.0 {
        return Err(MetricsError::InvalidValue(field));
    }
    Ok(value)
}

fn kib_to_bytes(kib: u64) -> Result<u64, MetricsError> {
    kib.checked_mul(1024).ok_or(MetricsError::CounterOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(cpu: &str, net: &str) -> String {
        format!(
            "LUMEN_METRICS_V1\ncpu={cpu}\nload=0.25,0.50,0.75\n\
             mem_kib=1000,400\ndisk_kib=2000,500\nnet={net}\nuptime=123.5\n"
        )
    }

    #[test]
    fn parses_linux_monitor_sample() {
        let mut accumulator = MetricsAccumulator::new();
        let metrics = accumulator
            .update(&sample("100,0,50,800,0,0,0,0", "1000,2000"), None)
            .expect("valid metrics");

        assert_eq!(metrics.cpu_usage_percent, None);
        assert_eq!(metrics.load_average_5m, 0.5);
        assert_eq!(metrics.memory.total_bytes, 1_024_000);
        assert_eq!(metrics.memory.used_bytes, 614_400);
        assert_eq!(metrics.root_disk.used_bytes, 1_536_000);
        assert_eq!(metrics.network.receive_bytes_per_second, None);
        assert_eq!(metrics.uptime_seconds, 123.5);
    }

    #[test]
    fn computes_cpu_and_network_deltas() {
        let mut accumulator = MetricsAccumulator::new();
        accumulator
            .update(&sample("100,0,50,800,0,0,0,0", "1000,2000"), None)
            .expect("first sample");
        let metrics = accumulator
            .update(
                &sample("150,0,70,880,0,0,0,0", "1600,2600"),
                Some(Duration::from_secs(2)),
            )
            .expect("second sample");

        let cpu = metrics.cpu_usage_percent.expect("CPU delta");
        assert!((cpu - 46.666_668).abs() < 0.001);
        assert_eq!(metrics.network.receive_bytes_per_second, Some(300.0));
        assert_eq!(metrics.network.transmit_bytes_per_second, Some(300.0));
    }

    #[test]
    fn counter_reset_does_not_create_false_spike() {
        let mut accumulator = MetricsAccumulator::new();
        accumulator
            .update(&sample("100,0,50,800,0,0,0,0", "1000,2000"), None)
            .expect("first sample");
        let metrics = accumulator
            .update(
                &sample("1,0,1,8,0,0,0,0", "10,20"),
                Some(Duration::from_secs(1)),
            )
            .expect("reset sample");
        assert_eq!(metrics.cpu_usage_percent, None);
        assert_eq!(metrics.network.receive_bytes_per_second, None);
        assert_eq!(metrics.network.transmit_bytes_per_second, None);
    }

    #[test]
    fn rejects_partial_or_unversioned_output() {
        let mut accumulator = MetricsAccumulator::new();
        assert_eq!(
            accumulator.update("cpu=1,2,3,4,5,6,7,8", None),
            Err(MetricsError::UnsupportedVersion)
        );
        assert_eq!(
            accumulator.update("LUMEN_METRICS_V1\ncpu=1,2,3,4,5,6,7,8\n", None),
            Err(MetricsError::MissingField("load"))
        );
    }

    #[test]
    fn command_has_no_interactive_or_privileged_operation() {
        assert!(LINUX_METRICS_COMMAND.contains("/proc/stat"));
        assert!(LINUX_METRICS_COMMAND.contains("df -PkP /"));
        assert!(!LINUX_METRICS_COMMAND.contains("sudo"));
        assert!(!LINUX_METRICS_COMMAND.contains("password"));
    }
}
