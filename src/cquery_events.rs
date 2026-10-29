//! Private event framing for the opt-in current-loading profile only. The outer
//! EVENTS section keeps its normal byte-string codec. No textual stripping or
//! inference of a phase from a diagnostic's contents is permitted.

const MAGIC: &[u8; 8] = b"BZCQEV01";
const HEADER: usize = 24;
// Keep the existing 8 MiB EVENTS bound, including framing and the outer codec.
const PAYLOAD_LIMIT: usize = (8 << 20) - 32;
const ROOT_MAGIC: &[u8; 8] = b"BZCQRC01";
const ROOT_HEADER: usize = 48;

pub(super) fn encode_root(
    prefix: &[u8],
    suffix: &[u8],
    cut: crate::engine::root_checkpoint::RootCut,
) -> Result<Vec<u8>, String> {
    if prefix.len().saturating_add(suffix.len()) > PAYLOAD_LIMIT - ROOT_HEADER {
        return Err("root-cut events exceed EVENTS limit".into());
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(ROOT_MAGIC);
    bytes.extend_from_slice(&cut.ordinal.to_le_bytes());
    bytes.extend_from_slice(&cut.registration);
    bytes.extend_from_slice(&encode(prefix, suffix)?);
    Ok(bytes)
}

pub(super) fn decode_root(
    bytes: &[u8],
    cut: crate::engine::root_checkpoint::RootCut,
) -> Result<Phases<'_>, String> {
    let header = bytes
        .get(..ROOT_HEADER)
        .ok_or("truncated root-cut event frame")?;
    if &header[..8] != ROOT_MAGIC
        || header[8..16] != cut.ordinal.to_le_bytes()
        || header[16..48] != cut.registration
    {
        return Err("candidate root-cut identity differs from current completed root".into());
    }
    if bytes.len() > HEADER + PAYLOAD_LIMIT {
        return Err("root-cut events exceed EVENTS limit".into());
    }
    decode(&bytes[ROOT_HEADER..])
}

pub(super) struct Phases<'a> {
    pub prefix: &'a [u8],
    pub suffix: &'a [u8],
}

/// Private-constructor agreement with the actual, already-emitted prefix. This
/// permits quiet *proof re-evaluation*, never extra command diagnostics. Stored
/// suffix output remains subject to every graph/input/adoption guard.
pub(crate) struct PrefixAgreement(());

impl Phases<'_> {
    pub(super) fn agree(&self, current: &[u8]) -> Result<PrefixAgreement, String> {
        if self.prefix == current {
            Ok(PrefixAgreement(()))
        } else {
            Err("current loading diagnostics differ from the recorded prefix".into())
        }
    }
}

pub(super) fn complete(capture: &crate::snapshot::capture::Captured) -> bool {
    capture.complete && !capture.overflowed && !capture.ended_early
}

pub(super) fn encode(prefix: &[u8], suffix: &[u8]) -> Result<Vec<u8>, String> {
    let size = prefix
        .len()
        .checked_add(suffix.len())
        .filter(|size| *size <= PAYLOAD_LIMIT)
        .ok_or("current-loading event phases exceed EVENTS limit")?;
    let mut bytes = Vec::with_capacity(HEADER + size);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(prefix.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&(suffix.len() as u64).to_le_bytes());
    bytes.extend_from_slice(prefix);
    bytes.extend_from_slice(suffix);
    Ok(bytes)
}

pub(super) fn decode(bytes: &[u8]) -> Result<Phases<'_>, String> {
    let header = bytes
        .get(..HEADER)
        .ok_or("truncated current-loading event phases")?;
    if &header[..8] != MAGIC {
        return Err("unsupported current-loading event phase format".into());
    }
    let length = |offset| -> Result<usize, String> {
        let length =
            u64::from_le_bytes(header[offset..offset + 8].try_into().expect("fixed header"));
        usize::try_from(length)
            .ok()
            .filter(|length| *length <= PAYLOAD_LIMIT)
            .ok_or_else(|| "invalid current-loading event phase length".into())
    };
    let prefix = length(8)?;
    let suffix = length(16)?;
    let total = prefix
        .checked_add(suffix)
        .filter(|total| *total <= PAYLOAD_LIMIT)
        .ok_or("current-loading event phases exceed EVENTS limit")?;
    if bytes.len() != HEADER + total {
        return Err("current-loading event phase lengths do not exhaust EVENTS".into());
    }
    Ok(Phases {
        prefix: &bytes[HEADER..HEADER + prefix],
        suffix: &bytes[HEADER + prefix..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_cut_identity_is_explicit_and_never_aliases_pattern_events() {
        let cut = crate::engine::root_checkpoint::RootCut {
            ordinal: 2,
            registration: [9; 32],
        };
        let bytes = encode_root(b"configured prefix", b"analysis suffix", cut).unwrap();
        assert!(decode(&bytes).is_err());
        assert!(
            decode_root(
                &encode(b"configured prefix", b"analysis suffix").unwrap(),
                cut
            )
            .is_err()
        );
        let phases = decode_root(&bytes, cut).unwrap();
        assert_eq!(phases.prefix, b"configured prefix");
        assert_eq!(phases.suffix, b"analysis suffix");
        for wrong in [
            crate::engine::root_checkpoint::RootCut { ordinal: 1, ..cut },
            crate::engine::root_checkpoint::RootCut {
                registration: [8; 32],
                ..cut
            },
        ] {
            assert!(decode_root(&bytes, wrong).is_err());
        }
        for end in 0..bytes.len() {
            assert!(decode_root(&bytes[..end], cut).is_err());
        }
    }

    #[test]
    fn a_phase_requires_complete_nonoverflowing_capture_even_if_its_bytes_match() {
        let mut captured = crate::snapshot::capture::Captured {
            bytes: b"same".to_vec(),
            complete: true,
            ..Default::default()
        };
        assert!(complete(&captured));
        captured.overflowed = true;
        assert!(!complete(&captured));
        captured.overflowed = false;
        captured.ended_early = true;
        assert!(!complete(&captured));
        captured.ended_early = false;
        captured.complete = false;
        assert!(!complete(&captured));
    }

    #[test]
    fn phase_ownership_is_explicit_even_when_concatenated_output_is_identical() {
        let first = encode(b"prefix\0\xff", b"suffix\n").unwrap();
        let parsed = decode(&first).unwrap();
        assert_eq!(parsed.prefix, b"prefix\0\xff");
        assert_eq!(parsed.suffix, b"suffix\n");
        assert_ne!(encode(b"a", b"bc").unwrap(), encode(b"ab", b"c").unwrap());
        assert_eq!(
            decode(&encode(b"", b"analysis only").unwrap())
                .unwrap()
                .prefix,
            b""
        );
    }

    #[test]
    fn malformed_oversized_or_unframed_events_are_never_diagnostics() {
        let valid = encode(b"p", b"s").unwrap();
        for end in 0..valid.len() {
            assert!(decode(&valid[..end]).is_err());
        }
        let mut extra = valid.clone();
        extra.push(0);
        assert!(decode(&extra).is_err());
        let mut version = valid.clone();
        version[7] = b'2';
        assert!(decode(&version).is_err());
        for offset in [8, 16] {
            let mut huge = valid.clone();
            huge[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            assert!(decode(&huge).is_err());
        }
        assert!(decode(b"old flat diagnostics").is_err());
        assert!(encode(&vec![0; PAYLOAD_LIMIT], b"x").is_err());
    }
}
