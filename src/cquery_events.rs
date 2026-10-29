//! Private event framing for the opt-in current-loading profile only. The outer
//! EVENTS section keeps its normal byte-string codec. No textual stripping or
//! inference of a phase from a diagnostic's contents is permitted.

const MAGIC: &[u8; 8] = b"BZCQEV01";
const HEADER: usize = 24;
// Keep the existing 8 MiB EVENTS bound, including framing and the outer codec.
const PAYLOAD_LIMIT: usize = (8 << 20) - 32;

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
