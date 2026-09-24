use aivtuber_runtime::SecurityRuntimeConfig;
use std::io::{self, BufRead};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawIngressConfig {
    pub max_record_bytes: usize,
    pub queue_capacity: usize,
}

impl RawIngressConfig {
    /// Keep pre-runtime bounds aligned with the downstream security boundary.
    pub fn from_security(config: SecurityRuntimeConfig) -> Result<Self, RawIngressError> {
        let raw = Self {
            max_record_bytes: config.max_envelope_bytes,
            queue_capacity: config.content_queue_limit,
        };
        raw.validate()?;
        Ok(raw)
    }

    pub fn validate(self) -> Result<(), RawIngressError> {
        if self.max_record_bytes == 0 {
            return Err(RawIngressError::InvalidConfiguration(
                "max_record_bytes must be positive",
            ));
        }
        if self.max_record_bytes == usize::MAX {
            return Err(RawIngressError::InvalidConfiguration(
                "max_record_bytes must leave room for one boundary byte",
            ));
        }
        if self.queue_capacity == 0 {
            return Err(RawIngressError::InvalidConfiguration(
                "queue_capacity must be positive",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawIngressMetricsSnapshot {
    pub enqueued: u64,
    pub dropped_queue_full: u64,
    pub dropped_oversize: u64,
    pub read_errors: u64,
    pub max_retained_record_bytes: u64,
}

#[derive(Debug, Default)]
struct RawIngressMetricState {
    enqueued: AtomicU64,
    dropped_queue_full: AtomicU64,
    dropped_oversize: AtomicU64,
    read_errors: AtomicU64,
    max_retained_record_bytes: AtomicU64,
}

#[derive(Debug, Clone, Default)]
pub struct RawIngressMetrics {
    state: Arc<RawIngressMetricState>,
}

impl RawIngressMetrics {
    pub fn snapshot(&self) -> RawIngressMetricsSnapshot {
        RawIngressMetricsSnapshot {
            enqueued: self.state.enqueued.load(Ordering::Relaxed),
            dropped_queue_full: self.state.dropped_queue_full.load(Ordering::Relaxed),
            dropped_oversize: self.state.dropped_oversize.load(Ordering::Relaxed),
            read_errors: self.state.read_errors.load(Ordering::Relaxed),
            max_retained_record_bytes: self.state.max_retained_record_bytes.load(Ordering::Relaxed),
        }
    }

    fn observe_retained(&self, bytes: usize) {
        let bytes = bytes.min(u64::MAX as usize) as u64;
        self.state
            .max_retained_record_bytes
            .fetch_max(bytes, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawIngressExit {
    Eof,
    ReceiverDisconnected,
}

#[derive(Debug)]
pub enum RawIngressError {
    InvalidConfiguration(&'static str),
    Io(io::Error),
}

impl std::fmt::Display for RawIngressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => {
                write!(f, "invalid raw ingress configuration: {message}")
            }
            Self::Io(error) => write!(f, "raw ingress I/O failed: {error}"),
        }
    }
}

impl std::error::Error for RawIngressError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidConfiguration(_) => None,
        }
    }
}

enum BoundedRecord {
    Eof,
    Record(Vec<u8>),
    Oversized,
}

/// Drain newline-delimited records into a bounded non-blocking handoff.
///
/// Queue saturation drops the newest raw record deterministically. Oversized
/// records are fully drained through their delimiter so the next record starts
/// at a clean boundary, while at most max_record_bytes + 1 bytes are retained.
pub fn pump_bounded_records<R: BufRead>(
    mut reader: R,
    sender: &SyncSender<Vec<u8>>,
    config: RawIngressConfig,
    metrics: &RawIngressMetrics,
) -> Result<RawIngressExit, RawIngressError> {
    config.validate()?;

    loop {
        let record = match read_bounded_record(&mut reader, config.max_record_bytes, metrics) {
            Ok(record) => record,
            Err(error) => {
                metrics.state.read_errors.fetch_add(1, Ordering::Relaxed);
                return Err(RawIngressError::Io(error));
            }
        };

        match record {
            BoundedRecord::Eof => return Ok(RawIngressExit::Eof),
            BoundedRecord::Oversized => {
                metrics
                    .state
                    .dropped_oversize
                    .fetch_add(1, Ordering::Relaxed);
            }
            BoundedRecord::Record(record) => match sender.try_send(record) {
                Ok(()) => {
                    metrics.state.enqueued.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    metrics
                        .state
                        .dropped_queue_full
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Ok(RawIngressExit::ReceiverDisconnected);
                }
            },
        }
    }
}

fn read_bounded_record<R: BufRead>(
    reader: &mut R,
    max_record_bytes: usize,
    metrics: &RawIngressMetrics,
) -> io::Result<BoundedRecord> {
    let retain_limit = max_record_bytes + 1;
    let mut retained = Vec::with_capacity(retain_limit.min(8 * 1024));
    let mut total_bytes = 0_usize;
    let mut saw_input = false;
    let mut definitely_oversized = false;

    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if !saw_input {
                return Ok(BoundedRecord::Eof);
            }
            return Ok(finish_record(
                retained,
                total_bytes,
                max_record_bytes,
                definitely_oversized,
            ));
        }

        saw_input = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let data_len = newline.unwrap_or(buffer.len());
        total_bytes = total_bytes.saturating_add(data_len);

        if !definitely_oversized {
            let remaining = retain_limit.saturating_sub(retained.len());
            let copy_len = data_len.min(remaining);
            retained.extend_from_slice(&buffer[..copy_len]);
            metrics.observe_retained(retained.len());
            if total_bytes > retain_limit {
                definitely_oversized = true;
            }
        }

        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        reader.consume(consumed);

        if newline.is_some() {
            return Ok(finish_record(
                retained,
                total_bytes,
                max_record_bytes,
                definitely_oversized,
            ));
        }
    }
}

fn finish_record(
    mut retained: Vec<u8>,
    total_bytes: usize,
    max_record_bytes: usize,
    definitely_oversized: bool,
) -> BoundedRecord {
    if definitely_oversized {
        return BoundedRecord::Oversized;
    }

    let had_cr = retained.last() == Some(&b'\r');
    if had_cr {
        retained.pop();
    }
    let logical_bytes = total_bytes.saturating_sub(usize::from(had_cr));
    if logical_bytes > max_record_bytes || retained.len() > max_record_bytes {
        BoundedRecord::Oversized
    } else {
        BoundedRecord::Record(retained)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};
    use std::sync::mpsc;

    fn config(max_record_bytes: usize, queue_capacity: usize) -> RawIngressConfig {
        RawIngressConfig {
            max_record_bytes,
            queue_capacity,
        }
    }

    #[test]
    fn queue_saturation_drops_newest_without_blocking_and_preserves_order() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let metrics = RawIngressMetrics::default();
        let input = Cursor::new(b"one\ntwo\nthree\nfour\n".to_vec());

        assert_eq!(
            pump_bounded_records(BufReader::new(input), &sender, config(16, 2), &metrics)
                .expect("pump"),
            RawIngressExit::Eof
        );
        drop(sender);

        assert_eq!(
            receiver.into_iter().collect::<Vec<_>>(),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.enqueued, 2);
        assert_eq!(snapshot.dropped_queue_full, 2);
        assert_eq!(snapshot.dropped_oversize, 0);
    }

    #[test]
    fn oversized_record_is_drained_without_retaining_the_full_line() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let metrics = RawIngressMetrics::default();
        let mut input = vec![b'x'; 256 * 1024];
        input.extend_from_slice(b"\nnot-json\n{}\n");

        pump_bounded_records(
            BufReader::with_capacity(17, Cursor::new(input)),
            &sender,
            config(32, 4),
            &metrics,
        )
        .expect("pump");
        drop(sender);

        assert_eq!(
            receiver.into_iter().collect::<Vec<_>>(),
            vec![b"not-json".to_vec(), b"{}".to_vec()]
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.dropped_oversize, 1);
        assert!(snapshot.max_retained_record_bytes <= 33);
    }

    #[test]
    fn byte_limit_handles_multibyte_utf8_and_recovers_after_oversize() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let metrics = RawIngressMetrics::default();
        let input = Cursor::new("éé\nééx\nok\n".as_bytes().to_vec());

        pump_bounded_records(BufReader::new(input), &sender, config(4, 8), &metrics).expect("pump");
        drop(sender);

        let records = receiver.into_iter().collect::<Vec<_>>();
        assert_eq!(records, vec!["éé".as_bytes().to_vec(), b"ok".to_vec()]);
        assert_eq!(metrics.snapshot().dropped_oversize, 1);
    }

    #[test]
    fn many_records_exactly_at_byte_limit_are_preserved_in_order() {
        let (sender, receiver) = mpsc::sync_channel(64);
        let metrics = RawIngressMetrics::default();
        let input = (0..32).map(|_| "1234\n").collect::<String>();

        pump_bounded_records(
            BufReader::new(Cursor::new(input.into_bytes())),
            &sender,
            config(4, 64),
            &metrics,
        )
        .expect("pump");
        drop(sender);

        let records = receiver.into_iter().collect::<Vec<_>>();
        assert_eq!(records.len(), 32);
        assert!(records.iter().all(|record| record == b"1234"));
        assert_eq!(metrics.snapshot().dropped_oversize, 0);
    }

    #[test]
    fn crlf_record_at_limit_matches_lines_style_delimiter_removal() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let metrics = RawIngressMetrics::default();
        let input = Cursor::new(b"1234\r\nnext\n".to_vec());

        pump_bounded_records(BufReader::new(input), &sender, config(4, 4), &metrics).expect("pump");
        drop(sender);

        assert_eq!(
            receiver.into_iter().collect::<Vec<_>>(),
            vec![b"1234".to_vec(), b"next".to_vec()]
        );
        assert_eq!(metrics.snapshot().dropped_oversize, 0);
    }

    #[test]
    fn saturated_queue_shutdown_does_not_block_reader() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .try_send(b"already-queued".to_vec())
            .expect("fill queue");
        drop(receiver);
        let metrics = RawIngressMetrics::default();
        let input = Cursor::new(b"one\ntwo\n".to_vec());

        assert_eq!(
            pump_bounded_records(BufReader::new(input), &sender, config(16, 1), &metrics)
                .expect("pump"),
            RawIngressExit::ReceiverDisconnected
        );
    }

    #[test]
    fn config_is_derived_from_security_runtime_bounds() {
        let security = SecurityRuntimeConfig {
            max_envelope_bytes: 4096,
            content_queue_limit: 17,
            ..SecurityRuntimeConfig::default()
        };
        assert_eq!(
            RawIngressConfig::from_security(security).expect("raw config"),
            RawIngressConfig {
                max_record_bytes: 4096,
                queue_capacity: 17,
            }
        );
    }
}
