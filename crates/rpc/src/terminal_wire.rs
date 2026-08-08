//! Compact binary terminal event codec used by `SubscribeTerminalV2`.

use bytes::{BufMut, Bytes, BytesMut};

use crate::{RpcError, binary_payload};

const VERSION: u8 = 1;
const DATA: u8 = 1;
const EXIT: u8 = 2;
const REPLAY_GAP: u8 = 3;
const HEADER_LEN: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalBinaryEvent {
    Data {
        seq: u64,
        data: Bytes,
    },
    Exit {
        seq: u64,
        exit_code: i32,
        signal: Option<String>,
    },
    ReplayGap {
        requested_after: u64,
        oldest_available: u64,
    },
}

pub fn encode_data(seq: u64, data: &[u8]) -> Bytes {
    let mut encoded = BytesMut::with_capacity(HEADER_LEN + data.len());
    encoded.put_slice(&[VERSION, DATA]);
    encoded.put_u64_le(seq);
    encoded.put_slice(data);
    encoded.freeze()
}

pub fn encode_exit(seq: u64, exit_code: i32, signal: Option<&str>) -> Bytes {
    let signal = signal.unwrap_or_default().as_bytes();
    let signal_len = u16::try_from(signal.len()).unwrap_or(u16::MAX);
    let signal = &signal[..usize::from(signal_len)];
    let mut encoded = BytesMut::with_capacity(HEADER_LEN + 6 + signal.len());
    encoded.put_slice(&[VERSION, EXIT]);
    encoded.put_u64_le(seq);
    encoded.put_i32_le(exit_code);
    encoded.put_u16_le(signal_len);
    encoded.put_slice(signal);
    encoded.freeze()
}

pub fn encode_replay_gap(requested_after: u64, oldest_available: u64) -> Bytes {
    let mut encoded = BytesMut::with_capacity(18);
    encoded.put_slice(&[VERSION, REPLAY_GAP]);
    encoded.put_u64_le(requested_after);
    encoded.put_u64_le(oldest_available);
    encoded.freeze()
}

pub fn decode(bytes: Bytes) -> Result<TerminalBinaryEvent, RpcError> {
    let version = *bytes.first().ok_or_else(|| malformed("missing version"))?;
    if version != VERSION {
        return Err(malformed("unsupported version"));
    }
    let kind = *bytes.get(1).ok_or_else(|| malformed("missing kind"))?;
    let seq = u64::from_le_bytes(
        binary_payload(&bytes, 2, 8, "terminal sequence")?
            .try_into()
            .map_err(|_| malformed("invalid sequence"))?,
    );
    match kind {
        DATA => Ok(TerminalBinaryEvent::Data {
            seq,
            data: bytes.slice(HEADER_LEN..),
        }),
        EXIT => {
            let exit_code = i32::from_le_bytes(
                binary_payload(&bytes, HEADER_LEN, 4, "terminal exit code")?
                    .try_into()
                    .map_err(|_| malformed("invalid exit code"))?,
            );
            let signal_len = u16::from_le_bytes(
                binary_payload(&bytes, HEADER_LEN + 4, 2, "terminal signal length")?
                    .try_into()
                    .map_err(|_| malformed("invalid signal length"))?,
            );
            let signal = binary_payload(
                &bytes,
                HEADER_LEN + 6,
                usize::from(signal_len),
                "terminal signal",
            )?;
            if HEADER_LEN + 6 + signal.len() != bytes.len() {
                return Err(malformed("trailing exit bytes"));
            }
            let signal = if signal.is_empty() {
                None
            } else {
                Some(
                    std::str::from_utf8(signal)
                        .map_err(|_| malformed("signal is not UTF-8"))?
                        .to_owned(),
                )
            };
            Ok(TerminalBinaryEvent::Exit {
                seq,
                exit_code,
                signal,
            })
        }
        REPLAY_GAP => {
            if bytes.len() != 18 {
                return Err(malformed("invalid replay gap length"));
            }
            let oldest_available = u64::from_le_bytes(
                binary_payload(&bytes, HEADER_LEN, 8, "oldest available sequence")?
                    .try_into()
                    .map_err(|_| malformed("invalid oldest available sequence"))?,
            );
            Ok(TerminalBinaryEvent::ReplayGap {
                requested_after: seq,
                oldest_available,
            })
        }
        _ => Err(malformed("unknown event kind")),
    }
}

fn malformed(detail: &str) -> RpcError {
    RpcError::Transport(format!("binary terminal event: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_round_trips_arbitrary_bytes() {
        let data = [0, 1, 2, 0x80, 0xff];
        assert_eq!(
            decode(encode_data(42, &data)).unwrap(),
            TerminalBinaryEvent::Data {
                seq: 42,
                data: Bytes::copy_from_slice(&data),
            }
        );
    }

    #[test]
    fn exit_round_trips_with_and_without_signal() {
        for signal in [None, Some("TERM")] {
            assert_eq!(
                decode(encode_exit(9, 143, signal)).unwrap(),
                TerminalBinaryEvent::Exit {
                    seq: 9,
                    exit_code: 143,
                    signal: signal.map(str::to_owned),
                }
            );
        }
    }

    #[test]
    fn replay_gap_round_trips() {
        assert_eq!(
            decode(encode_replay_gap(7, 19)).unwrap(),
            TerminalBinaryEvent::ReplayGap {
                requested_after: 7,
                oldest_available: 19,
            }
        );
    }

    #[test]
    fn malformed_frames_are_rejected() {
        assert!(decode(Bytes::new()).is_err());
        assert!(decode(Bytes::from_static(&[2, DATA])).is_err());
        assert!(decode(Bytes::from_static(&[VERSION, 99, 0, 0, 0, 0, 0, 0, 0, 0,])).is_err());
        let mut exit = BytesMut::from(encode_exit(1, 0, None).as_ref());
        exit.put_u8(1);
        assert!(decode(exit.freeze()).is_err());
    }
}
