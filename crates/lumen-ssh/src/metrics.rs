use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub(crate) const LINUX_METRICS_COMMAND: &str = r#"LC_ALL=C; export LC_ALL; printf 'LUMEN_METRICS_V2\n'; sanitize_lumen_text() { printf '%s' "$1" | tr -cd 'A-Za-z0-9 ._+:/()@-' | cut -c 1-128; }; lumen_system=$(awk -F= '$1 == "PRETTY_NAME" {value=substr($0, index($0, "=") + 1); sub(/^"/, "", value); sub(/"$/, "", value); print value; exit}' /etc/os-release 2>/dev/null); [ -n "$lumen_system" ] || lumen_system=$(uname -s 2>/dev/null); lumen_kernel=$(uname -r 2>/dev/null); lumen_timezone=$(awk 'NR == 1 {print; exit}' /etc/timezone 2>/dev/null); if [ -z "$lumen_timezone" ]; then lumen_timezone=$(readlink /etc/localtime 2>/dev/null | sed 's#^.*/zoneinfo/##'); fi; [ -n "$lumen_timezone" ] || lumen_timezone=$(date +%Z 2>/dev/null); printf 'system_name=%s\n' "$(sanitize_lumen_text "$lumen_system")"; printf 'kernel_version=%s\n' "$(sanitize_lumen_text "$lumen_kernel")"; printf 'timezone=%s\n' "$(sanitize_lumen_text "$lumen_timezone")"; awk 'BEGIN {cores=0} /^cpu / {print "cpu=" $2 "," $3 "," $4 "," $5 "," $6 "," $7 "," $8 "," $9} /^cpu[0-9]+ / {cores++; if (cores <= 256) {core_index=substr($1, 4); print "cpu_core=" core_index "," $2 "," $3 "," $4 "," $5 "," $6 "," $7 "," $8 "," $9}} END {print "cpu_count=" cores}' /proc/stat; awk '{print "load=" $1 "," $2 "," $3; exit}' /proc/loadavg; awk '/^MemTotal:/ {total=$2} /^MemAvailable:/ {available=$2} /^Cached:/ {cached=$2} /^SReclaimable:/ {reclaim=$2} /^Shmem:/ {shmem=$2} END {cache=(cached+reclaim >= shmem ? cached+reclaim-shmem : 0); if (total != "" && available != "") printf "mem_kib=%.0f,%.0f,%.0f\n", total, cache, available}' /proc/meminfo; df -PTkP / 2>/dev/null | awk 'NR == 2 {print "disk_kib=" $2 "," $3 "," $5; exit}'; awk 'NF >= 14 && $3 ~ /^(sd[a-z]+|hd[a-z]+|vd[a-z]+|xvd[a-z]+|dasd[a-z]+|nvme[0-9]+n[0-9]+|mmcblk[0-9]+)$/ {devices++; if (devices <= 128) {read_sectors += $6; written_sectors += $10}} END {if (devices > 0 && devices <= 128) printf "disk_io=%.0f,%.0f\n", read_sectors * 512, written_sectors * 512}' /proc/diskstats 2>/dev/null; awk 'NR > 2 {line=$0; sub(/^[[:space:]]+/, "", line); count=split(line, value, /[:[:space:]]+/); if (count >= 10 && value[1] != "lo") {rx += value[2]; tx += value[10]}} END {printf "net=%.0f,%.0f\n", rx+0, tx+0}' /proc/net/dev; ps -eo pid=,pcpu=,pmem=,comm= --sort=-pcpu 2>/dev/null | awk 'NR <= 8 && NF >= 4 {command=$4; for (i=5; i<=NF; i++) command=command " " $i; gsub(/[[:cntrl:]]/, " ", command); command=substr(command, 1, 256); print "process=" $1 "," $2 "," $3 "," command}'; awk '{print "uptime=" $1; exit}' /proc/uptime"#;

const MAX_METRICS_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_CPU_CORES: usize = 256;
const MAX_PROCESSES: usize = 8;
const MAX_SYSTEM_TEXT_BYTES: usize = 128;
const MAX_FILESYSTEM_TYPE_BYTES: usize = 64;
const MAX_PROCESS_COMMAND_BYTES: usize = 256;

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
    /// Extended fields emitted by the bounded V2 Linux collector.
    ///
    /// Old V1 samples and serialized snapshots deserialize with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<ServerMonitorDetails>>,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerMonitorDetails {
    pub system_name: String,
    pub kernel_version: String,
    pub timezone: String,
    pub logical_cpu_count: u32,
    pub cpu_cores: Vec<CpuCoreMetrics>,
    pub memory_cached_bytes: u64,
    pub root_filesystem_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_io: Option<DiskIoMetrics>,
    pub processes: Vec<ProcessMetrics>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskIoMetrics {
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub read_bytes_per_second: Option<f64>,
    pub write_bytes_per_second: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CpuCoreMetrics {
    pub logical_index: u32,
    pub usage_percent: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessMetrics {
    pub pid: u32,
    pub cpu_usage_percent: f32,
    pub memory_usage_percent: f32,
    pub command: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CpuCounterSnapshot {
    logical_index: u32,
    total: u64,
    idle: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CounterSnapshot {
    cpu_total: u64,
    cpu_idle: u64,
    cpu_cores: Vec<CpuCounterSnapshot>,
    disk_io: Option<(u64, u64)>,
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
            cpu_cores: raw
                .cpu_cores
                .iter()
                .map(|core| CpuCounterSnapshot {
                    logical_index: core.logical_index,
                    total: core.total,
                    idle: core.idle,
                })
                .collect(),
            disk_io: raw.disk_io,
            received_bytes: raw.received_bytes,
            transmitted_bytes: raw.transmitted_bytes,
        };

        let cpu_usage_percent = self.previous.as_ref().and_then(|previous| {
            counter_usage_percent(
                current.cpu_total,
                current.cpu_idle,
                previous.cpu_total,
                previous.cpu_idle,
            )
        });

        let cpu_cores = raw
            .cpu_cores
            .iter()
            .map(|core| {
                let usage_percent = self.previous.as_ref().and_then(|previous| {
                    let index = previous
                        .cpu_cores
                        .binary_search_by_key(&core.logical_index, |item| item.logical_index)
                        .ok()?;
                    let previous = previous.cpu_cores[index];
                    counter_usage_percent(core.total, core.idle, previous.total, previous.idle)
                });
                CpuCoreMetrics {
                    logical_index: core.logical_index,
                    usage_percent,
                }
            })
            .collect();

        let rate_seconds = elapsed
            .map(|duration| duration.as_secs_f64())
            .filter(|value| *value > 0.0);
        let (receive_bytes_per_second, transmit_bytes_per_second) =
            match (self.previous.as_ref(), rate_seconds) {
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

        let (disk_read_bytes_per_second, disk_write_bytes_per_second) =
            match (self.previous.as_ref(), current.disk_io, rate_seconds) {
                (Some(previous), Some((read_bytes, written_bytes)), Some(seconds)) => {
                    match previous.disk_io {
                        Some((previous_read_bytes, previous_written_bytes)) => (
                            read_bytes
                                .checked_sub(previous_read_bytes)
                                .map(|delta| delta as f64 / seconds),
                            written_bytes
                                .checked_sub(previous_written_bytes)
                                .map(|delta| delta as f64 / seconds),
                        ),
                        None => (None, None),
                    }
                }
                _ => (None, None),
            };

        self.previous = Some(current);

        let disk_io = raw
            .disk_io
            .map(|(read_bytes, written_bytes)| DiskIoMetrics {
                read_bytes,
                written_bytes,
                read_bytes_per_second: disk_read_bytes_per_second,
                write_bytes_per_second: disk_write_bytes_per_second,
            });
        let details = raw
            .details
            .map(|details| {
                let memory_cached_bytes = kib_to_bytes(details.memory_cached_kib)?;
                Ok(Box::new(ServerMonitorDetails {
                    system_name: details.system_name,
                    kernel_version: details.kernel_version,
                    timezone: details.timezone,
                    logical_cpu_count: details.logical_cpu_count,
                    cpu_cores,
                    memory_cached_bytes,
                    root_filesystem_type: details.root_filesystem_type,
                    disk_io,
                    processes: details.processes,
                }))
            })
            .transpose()?;

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
            details,
        })
    }
}

fn counter_usage_percent(
    current_total: u64,
    current_idle: u64,
    previous_total: u64,
    previous_idle: u64,
) -> Option<f32> {
    let total_delta = current_total.checked_sub(previous_total)?;
    let idle_delta = current_idle.checked_sub(previous_idle)?;
    if total_delta == 0 || idle_delta > total_delta {
        return None;
    }
    let busy_delta = total_delta - idle_delta;
    Some((busy_delta as f64 * 100.0 / total_delta as f64) as f32)
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
    #[error("monitor output contains too many {0}")]
    TooManyItems(&'static str),
    #[error("monitor counter overflow")]
    CounterOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetricsVersion {
    V1,
    V2,
}

#[derive(Clone, Copy, Debug)]
struct RawCpuCore {
    logical_index: u32,
    total: u64,
    idle: u64,
}

#[derive(Clone, Debug)]
struct RawMonitorDetails {
    system_name: String,
    kernel_version: String,
    timezone: String,
    logical_cpu_count: u32,
    memory_cached_kib: u64,
    root_filesystem_type: String,
    processes: Vec<ProcessMetrics>,
}

#[derive(Clone, Debug)]
struct RawMetrics {
    cpu_total: u64,
    cpu_idle: u64,
    cpu_cores: Vec<RawCpuCore>,
    load_average: [f64; 3],
    memory_total_kib: u64,
    memory_available_kib: u64,
    disk_total_kib: u64,
    disk_available_kib: u64,
    disk_io: Option<(u64, u64)>,
    received_bytes: u64,
    transmitted_bytes: u64,
    uptime_seconds: f64,
    details: Option<RawMonitorDetails>,
}

impl RawMetrics {
    fn parse(output: &str) -> Result<Self, MetricsError> {
        if output.len() > MAX_METRICS_OUTPUT_BYTES {
            return Err(MetricsError::OutputTooLarge);
        }

        let mut lines = output.lines();
        let version = match lines.next() {
            Some("LUMEN_METRICS_V1") => MetricsVersion::V1,
            Some("LUMEN_METRICS_V2") => MetricsVersion::V2,
            _ => return Err(MetricsError::UnsupportedVersion),
        };

        let mut cpu = None;
        let mut cpu_count = None;
        let mut cpu_cores = Vec::new();
        let mut load = None;
        let mut memory = None;
        let mut disk = None;
        let mut disk_io = None;
        let mut network = None;
        let mut uptime = None;
        let mut system_name = None;
        let mut kernel_version = None;
        let mut timezone = None;
        let mut processes = Vec::new();

        for line in lines {
            if line.is_empty() || line.trim() != line {
                return Err(MetricsError::UnknownField);
            }
            let (key, value) = line.split_once('=').ok_or(MetricsError::UnknownField)?;
            match key {
                "cpu" => set_once(&mut cpu, parse_cpu(value)?)?,
                "cpu_count" if version == MetricsVersion::V2 => {
                    set_once(&mut cpu_count, parse_cpu_count(value)?)?;
                }
                "cpu_core" if version == MetricsVersion::V2 => {
                    if cpu_cores.len() >= MAX_CPU_CORES {
                        return Err(MetricsError::TooManyItems("CPU cores"));
                    }
                    cpu_cores.push(parse_cpu_core(value)?);
                }
                "load" => set_once(&mut load, parse_float_triplet(value, "load")?)?,
                "mem_kib" => {
                    let parsed = match version {
                        MetricsVersion::V1 => {
                            let (total, available) = parse_u64_pair(value, "memory")?;
                            (total, None, available)
                        }
                        MetricsVersion::V2 => {
                            let values = parse_u64_list::<3>(value, "memory")?;
                            (values[0], Some(values[1]), values[2])
                        }
                    };
                    set_once(&mut memory, parsed)?;
                }
                "disk_kib" => {
                    let parsed = match version {
                        MetricsVersion::V1 => {
                            let (total, available) = parse_u64_pair(value, "disk")?;
                            (None, total, available)
                        }
                        MetricsVersion::V2 => parse_disk(value)?,
                    };
                    set_once(&mut disk, parsed)?;
                }
                "disk_io" if version == MetricsVersion::V2 => {
                    set_once(&mut disk_io, parse_u64_pair(value, "disk I/O")?)?;
                }
                "net" => {
                    set_once(&mut network, parse_u64_pair(value, "network")?)?;
                }
                "system_name" if version == MetricsVersion::V2 => set_once(
                    &mut system_name,
                    parse_bounded_text(value, MAX_SYSTEM_TEXT_BYTES, "system name")?,
                )?,
                "kernel_version" if version == MetricsVersion::V2 => set_once(
                    &mut kernel_version,
                    parse_bounded_text(value, MAX_SYSTEM_TEXT_BYTES, "kernel version")?,
                )?,
                "timezone" if version == MetricsVersion::V2 => set_once(
                    &mut timezone,
                    parse_bounded_text(value, MAX_SYSTEM_TEXT_BYTES, "timezone")?,
                )?,
                "process" if version == MetricsVersion::V2 => {
                    if processes.len() >= MAX_PROCESSES {
                        return Err(MetricsError::TooManyItems("processes"));
                    }
                    let process = parse_process(value)?;
                    if processes
                        .iter()
                        .any(|existing: &ProcessMetrics| existing.pid == process.pid)
                    {
                        return Err(MetricsError::InvalidValue("process"));
                    }
                    processes.push(process);
                }
                "uptime" => set_once(&mut uptime, parse_nonnegative_float(value, "uptime")?)?,
                _ => return Err(MetricsError::UnknownField),
            }
        }

        let (cpu_total, cpu_idle) = cpu.ok_or(MetricsError::MissingField("cpu"))?;
        let load_average = load.ok_or(MetricsError::MissingField("load"))?;
        let (memory_total_kib, memory_available_kib) = memory
            .map(|(total, _, available)| (total, available))
            .ok_or(MetricsError::MissingField("memory"))?;
        let (_, memory_cached_kib, _) = memory.ok_or(MetricsError::MissingField("memory"))?;
        let (root_filesystem_type, disk_total_kib, disk_available_kib) =
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
        if memory_cached_kib.is_some_and(|cached| cached > memory_total_kib) {
            return Err(MetricsError::InvalidValue("memory"));
        }

        let details = match version {
            MetricsVersion::V1 => None,
            MetricsVersion::V2 => {
                let logical_cpu_count = cpu_count.ok_or(MetricsError::MissingField("CPU count"))?;
                if usize::try_from(logical_cpu_count).ok() != Some(cpu_cores.len()) {
                    return Err(MetricsError::InvalidValue("CPU cores"));
                }
                if cpu_cores
                    .windows(2)
                    .any(|pair| pair[0].logical_index >= pair[1].logical_index)
                {
                    return Err(MetricsError::InvalidValue("CPU cores"));
                }
                let maximum_process_cpu = logical_cpu_count as f32 * 100.0;
                if processes
                    .iter()
                    .any(|process| process.cpu_usage_percent > maximum_process_cpu)
                {
                    return Err(MetricsError::InvalidValue("process CPU"));
                }
                Some(RawMonitorDetails {
                    system_name: system_name.ok_or(MetricsError::MissingField("system name"))?,
                    kernel_version: kernel_version
                        .ok_or(MetricsError::MissingField("kernel version"))?,
                    timezone: timezone.ok_or(MetricsError::MissingField("timezone"))?,
                    logical_cpu_count,
                    memory_cached_kib: memory_cached_kib
                        .ok_or(MetricsError::MissingField("memory cache"))?,
                    root_filesystem_type: root_filesystem_type
                        .ok_or(MetricsError::MissingField("filesystem type"))?,
                    processes,
                })
            }
        };

        Ok(Self {
            cpu_total,
            cpu_idle,
            cpu_cores,
            load_average,
            memory_total_kib,
            memory_available_kib,
            disk_total_kib,
            disk_available_kib,
            disk_io,
            received_bytes,
            transmitted_bytes,
            uptime_seconds,
            details,
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
    cpu_counter_totals(values, "cpu")
}

fn parse_cpu_count(value: &str) -> Result<u32, MetricsError> {
    let count = value
        .parse::<u32>()
        .map_err(|_| MetricsError::InvalidValue("CPU count"))?;
    if count == 0 {
        return Err(MetricsError::InvalidValue("CPU count"));
    }
    if usize::try_from(count).map_or(true, |count| count > MAX_CPU_CORES) {
        return Err(MetricsError::TooManyItems("CPU cores"));
    }
    Ok(count)
}

fn parse_cpu_core(value: &str) -> Result<RawCpuCore, MetricsError> {
    let (logical_index, counters) = value
        .split_once(',')
        .ok_or(MetricsError::InvalidValue("CPU core"))?;
    let logical_index = logical_index
        .parse::<u32>()
        .map_err(|_| MetricsError::InvalidValue("CPU core"))?;
    let values = parse_u64_list::<8>(counters, "CPU core")?;
    let (total, idle) = cpu_counter_totals(values, "CPU core")?;
    Ok(RawCpuCore {
        logical_index,
        total,
        idle,
    })
}

fn cpu_counter_totals(values: [u64; 8], field: &'static str) -> Result<(u64, u64), MetricsError> {
    let total = values
        .iter()
        .try_fold(0_u64, |sum, value| sum.checked_add(*value))
        .ok_or(MetricsError::CounterOverflow)?;
    let idle = values[3]
        .checked_add(values[4])
        .ok_or(MetricsError::CounterOverflow)?;
    if idle > total {
        return Err(MetricsError::InvalidValue(field));
    }
    Ok((total, idle))
}

fn parse_disk(value: &str) -> Result<(Option<String>, u64, u64), MetricsError> {
    let mut parts = value.split(',');
    let filesystem_type = parts
        .next()
        .ok_or(MetricsError::InvalidValue("filesystem type"))?;
    let filesystem_type = parse_filesystem_type(filesystem_type)?;
    let total = parse_u64(
        parts.next().ok_or(MetricsError::InvalidValue("disk"))?,
        "disk",
    )?;
    let available = parse_u64(
        parts.next().ok_or(MetricsError::InvalidValue("disk"))?,
        "disk",
    )?;
    if parts.next().is_some() {
        return Err(MetricsError::InvalidValue("disk"));
    }
    Ok((Some(filesystem_type), total, available))
}

fn parse_filesystem_type(value: &str) -> Result<String, MetricsError> {
    if value.is_empty()
        || value.len() > MAX_FILESYSTEM_TYPE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
    {
        return Err(MetricsError::InvalidValue("filesystem type"));
    }
    Ok(value.to_owned())
}

fn parse_process(value: &str) -> Result<ProcessMetrics, MetricsError> {
    let mut parts = value.splitn(4, ',');
    let pid = parts
        .next()
        .ok_or(MetricsError::InvalidValue("process"))?
        .parse::<u32>()
        .map_err(|_| MetricsError::InvalidValue("process"))?;
    if pid == 0 {
        return Err(MetricsError::InvalidValue("process"));
    }
    let cpu_usage_percent = parse_bounded_percent(
        parts
            .next()
            .ok_or(MetricsError::InvalidValue("process CPU"))?,
        f32::MAX,
        "process CPU",
    )?;
    let memory_usage_percent = parse_bounded_percent(
        parts
            .next()
            .ok_or(MetricsError::InvalidValue("process memory"))?,
        100.0,
        "process memory",
    )?;
    let command = parse_bounded_text(
        parts
            .next()
            .ok_or(MetricsError::InvalidValue("process command"))?,
        MAX_PROCESS_COMMAND_BYTES,
        "process command",
    )?;
    Ok(ProcessMetrics {
        pid,
        cpu_usage_percent,
        memory_usage_percent,
        command,
    })
}

fn parse_bounded_percent(
    value: &str,
    maximum: f32,
    field: &'static str,
) -> Result<f32, MetricsError> {
    let value = parse_nonnegative_float(value, field)?;
    if value > f64::from(maximum) {
        return Err(MetricsError::InvalidValue(field));
    }
    Ok(value as f32)
}

fn parse_bounded_text(
    value: &str,
    maximum_bytes: usize,
    field: &'static str,
) -> Result<String, MetricsError> {
    if value.is_empty()
        || value.len() > maximum_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(MetricsError::InvalidValue(field));
    }
    Ok(value.to_owned())
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

fn parse_u64(value: &str, field: &'static str) -> Result<u64, MetricsError> {
    value
        .parse::<u64>()
        .map_err(|_| MetricsError::InvalidValue(field))
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

    fn detailed_sample(cpu: &str, core_zero: &str, core_one: &str, net: &str) -> String {
        format!(
            "LUMEN_METRICS_V2\n\
             system_name=Ubuntu 24.04 LTS\n\
             kernel_version=6.8.0-31-generic\n\
             timezone=Asia/Shanghai\n\
             cpu={cpu}\n\
             cpu_core=0,{core_zero}\n\
             cpu_core=1,{core_one}\n\
             cpu_count=2\n\
             load=0.25,0.50,0.75\n\
             mem_kib=1000,300,400\n\
             disk_kib=ext4,2000,500\n\
             disk_io=4096,8192\n\
             net={net}\n\
             process=1,1.5,0.5,/sbin/init\n\
             process=42,150.0,12.5,worker --flag=a,b\n\
             uptime=123.5\n"
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
        assert!(metrics.details.is_none());
    }

    #[test]
    fn parses_all_bounded_v2_monitor_details() {
        let mut accumulator = MetricsAccumulator::new();
        let metrics = accumulator
            .update(
                &detailed_sample(
                    "100,0,50,800,0,0,0,0",
                    "50,0,25,400,0,0,0,0",
                    "50,0,25,400,0,0,0,0",
                    "1000,2000",
                ),
                None,
            )
            .expect("valid detailed metrics");
        let details = metrics.details.expect("V2 details");

        assert_eq!(details.system_name, "Ubuntu 24.04 LTS");
        assert_eq!(details.kernel_version, "6.8.0-31-generic");
        assert_eq!(details.timezone, "Asia/Shanghai");
        assert_eq!(details.logical_cpu_count, 2);
        assert_eq!(details.cpu_cores.len(), 2);
        assert_eq!(details.cpu_cores[0].logical_index, 0);
        assert_eq!(details.cpu_cores[0].usage_percent, None);
        assert_eq!(details.memory_cached_bytes, 307_200);
        assert_eq!(details.root_filesystem_type, "ext4");
        assert_eq!(
            details.disk_io,
            Some(DiskIoMetrics {
                read_bytes: 4096,
                written_bytes: 8192,
                read_bytes_per_second: None,
                write_bytes_per_second: None,
            })
        );
        assert_eq!(details.processes.len(), 2);
        assert_eq!(details.processes[0].pid, 1);
        assert_eq!(details.processes[1].command, "worker --flag=a,b");
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
    fn computes_disk_io_rates_and_handles_resets_and_missing_samples() {
        let mut accumulator = MetricsAccumulator::new();
        let first = detailed_sample(
            "100,0,50,800,0,0,0,0",
            "50,0,25,400,0,0,0,0",
            "50,0,25,400,0,0,0,0",
            "1000,2000",
        );
        accumulator.update(&first, None).expect("first sample");

        let second = first.replace("disk_io=4096,8192", "disk_io=4696,9192");
        let metrics = accumulator
            .update(&second, Some(Duration::from_secs(2)))
            .expect("second sample");
        let disk_io = metrics
            .details
            .expect("V2 details")
            .disk_io
            .expect("disk I/O");
        assert_eq!(disk_io.read_bytes, 4696);
        assert_eq!(disk_io.written_bytes, 9192);
        assert_eq!(disk_io.read_bytes_per_second, Some(300.0));
        assert_eq!(disk_io.write_bytes_per_second, Some(500.0));

        let reset = first.replace("disk_io=4096,8192", "disk_io=100,10192");
        let metrics = accumulator
            .update(&reset, Some(Duration::from_secs(1)))
            .expect("counter reset sample");
        let disk_io = metrics
            .details
            .expect("V2 details")
            .disk_io
            .expect("disk I/O");
        assert_eq!(disk_io.read_bytes_per_second, None);
        assert_eq!(disk_io.write_bytes_per_second, Some(1000.0));

        let missing = first.replace("disk_io=4096,8192\n", "");
        let metrics = accumulator
            .update(&missing, Some(Duration::from_secs(1)))
            .expect("sample without disk I/O");
        assert_eq!(metrics.details.expect("V2 details").disk_io, None);

        let recovered = accumulator
            .update(&first, Some(Duration::from_secs(1)))
            .expect("recovered sample");
        let disk_io = recovered
            .details
            .expect("V2 details")
            .disk_io
            .expect("disk I/O");
        assert_eq!(disk_io.read_bytes_per_second, None);
        assert_eq!(disk_io.write_bytes_per_second, None);
    }

    #[test]
    fn computes_per_core_deltas_by_logical_index() {
        let mut accumulator = MetricsAccumulator::new();
        accumulator
            .update(
                &detailed_sample(
                    "100,0,50,800,0,0,0,0",
                    "50,0,25,400,0,0,0,0",
                    "50,0,25,400,0,0,0,0",
                    "1000,2000",
                ),
                None,
            )
            .expect("first sample");
        let metrics = accumulator
            .update(
                &detailed_sample(
                    "150,0,70,880,0,0,0,0",
                    "80,0,35,440,0,0,0,0",
                    "70,0,35,440,0,0,0,0",
                    "1600,2600",
                ),
                Some(Duration::from_secs(2)),
            )
            .expect("second sample");
        let cores = &metrics.details.expect("V2 details").cpu_cores;

        assert_eq!(cores[0].usage_percent, Some(50.0));
        let second = cores[1].usage_percent.expect("second core delta");
        assert!((second - 42.857_143).abs() < 0.001);
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
    fn detailed_parser_fails_closed_on_counts_text_and_cross_field_bounds() {
        let base = detailed_sample(
            "100,0,50,800,0,0,0,0",
            "50,0,25,400,0,0,0,0",
            "50,0,25,400,0,0,0,0",
            "1000,2000",
        );
        let mut accumulator = MetricsAccumulator::new();

        let wrong_core_count = base.replace("cpu_count=2", "cpu_count=3");
        assert_eq!(
            accumulator.update(&wrong_core_count, None),
            Err(MetricsError::InvalidValue("CPU cores"))
        );

        let duplicate_core = base.replace(
            "cpu_core=1,50,0,25,400,0,0,0,0",
            "cpu_core=0,50,0,25,400,0,0,0,0",
        );
        assert_eq!(
            accumulator.update(&duplicate_core, None),
            Err(MetricsError::InvalidValue("CPU cores"))
        );

        let excessive_cache = base.replace("mem_kib=1000,300,400", "mem_kib=1000,1001,400");
        assert_eq!(
            accumulator.update(&excessive_cache, None),
            Err(MetricsError::InvalidValue("memory"))
        );

        let invalid_filesystem = base.replace("disk_kib=ext4,2000,500", "disk_kib=ext4!,2000,500");
        assert_eq!(
            accumulator.update(&invalid_filesystem, None),
            Err(MetricsError::InvalidValue("filesystem type"))
        );

        let long_command = base.replace("/sbin/init", &"x".repeat(MAX_PROCESS_COMMAND_BYTES + 1));
        assert_eq!(
            accumulator.update(&long_command, None),
            Err(MetricsError::InvalidValue("process command"))
        );

        let duplicate_pid = base.replace("process=42,150.0,12.5", "process=1,150.0,12.5");
        assert_eq!(
            accumulator.update(&duplicate_pid, None),
            Err(MetricsError::InvalidValue("process"))
        );

        let padded_system = base.replace("system_name=Ubuntu", "system_name= Ubuntu");
        assert_eq!(
            accumulator.update(&padded_system, None),
            Err(MetricsError::InvalidValue("system name"))
        );

        let blank_line = base.replace("kernel_version=", "\nkernel_version=");
        assert_eq!(
            accumulator.update(&blank_line, None),
            Err(MetricsError::UnknownField)
        );
    }

    #[test]
    fn detailed_parser_rejects_unbounded_process_and_output_counts() {
        let base = detailed_sample(
            "100,0,50,800,0,0,0,0",
            "50,0,25,400,0,0,0,0",
            "50,0,25,400,0,0,0,0",
            "1000,2000",
        );
        let extra_processes = (0..MAX_PROCESSES)
            .map(|index| format!("process={},0.0,0.0,worker-{index}\n", index + 100))
            .collect::<String>();
        let excessive_processes =
            base.replace("uptime=123.5", &format!("{extra_processes}uptime=123.5"));
        let mut accumulator = MetricsAccumulator::new();
        assert_eq!(
            accumulator.update(&excessive_processes, None),
            Err(MetricsError::TooManyItems("processes"))
        );

        let oversized = format!(
            "LUMEN_METRICS_V2\nsystem_name={}\n",
            "x".repeat(MAX_METRICS_OUTPUT_BYTES)
        );
        assert_eq!(
            accumulator.update(&oversized, None),
            Err(MetricsError::OutputTooLarge)
        );
    }

    #[test]
    fn disk_io_field_is_optional_but_malformed_or_duplicate_values_fail_closed() {
        let base = detailed_sample(
            "100,0,50,800,0,0,0,0",
            "50,0,25,400,0,0,0,0",
            "50,0,25,400,0,0,0,0",
            "1000,2000",
        );
        let mut accumulator = MetricsAccumulator::new();

        let missing = base.replace("disk_io=4096,8192\n", "");
        let metrics = accumulator
            .update(&missing, None)
            .expect("disk I/O may be unavailable");
        assert_eq!(metrics.details.expect("V2 details").disk_io, None);

        let malformed = base.replace("disk_io=4096,8192", "disk_io=4096,-1");
        assert_eq!(
            accumulator.update(&malformed, None),
            Err(MetricsError::InvalidValue("disk I/O"))
        );

        let duplicate = base.replace("disk_io=4096,8192", "disk_io=4096,8192\ndisk_io=1,2");
        assert_eq!(
            accumulator.update(&duplicate, None),
            Err(MetricsError::DuplicateField)
        );

        let v1_with_disk_io = sample("100,0,50,800,0,0,0,0", "1000,2000")
            .replace("net=1000,2000", "disk_io=1,2\nnet=1000,2000");
        assert_eq!(
            accumulator.update(&v1_with_disk_io, None),
            Err(MetricsError::UnknownField)
        );
    }

    #[test]
    fn command_has_no_interactive_or_privileged_operation() {
        assert!(LINUX_METRICS_COMMAND.contains("LUMEN_METRICS_V2"));
        assert!(LINUX_METRICS_COMMAND.contains("/proc/stat"));
        assert!(LINUX_METRICS_COMMAND.contains("df -PTkP /"));
        assert!(LINUX_METRICS_COMMAND.contains("/proc/diskstats"));
        assert!(LINUX_METRICS_COMMAND.contains("devices <= 128"));
        assert!(LINUX_METRICS_COMMAND.contains("disk_io=%.0f,%.0f"));
        assert!(LINUX_METRICS_COMMAND.contains("NR <= 8"));
        assert!(LINUX_METRICS_COMMAND.contains("cores <= 256"));
        assert!(LINUX_METRICS_COMMAND.contains("substr(command, 1, 256)"));
        assert!(LINUX_METRICS_COMMAND.contains("pmem=,comm="));
        assert!(!LINUX_METRICS_COMMAND.contains("args="));
        assert!(!LINUX_METRICS_COMMAND.contains("sudo"));
        assert!(!LINUX_METRICS_COMMAND.contains("password"));
        assert!(!LINUX_METRICS_COMMAND.contains("watch "));
        assert!(!LINUX_METRICS_COMMAND.contains("top "));
        assert!(!LINUX_METRICS_COMMAND.contains("kill "));
    }
}
