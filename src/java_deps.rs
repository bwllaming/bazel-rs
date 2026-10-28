//! Bounded reader for Bazel 9.3's proto2 `deps.proto`.
//!
//! Reduction uses every Dependency.path, regardless of kind or success. This
//! reader still validates required fields in dependencies and source locations,
//! as `Dependencies.parseFrom` does. Unknown fields/groups are skipped, wrong
//! wire types are unknown fields, repeated singular values use the last value,
//! and unknown enum values do not set (or clear) the required kind field.
//!
//! No paths from this message are opened or normalized: they are only compared
//! with the action's declared ordered classpath. Invalid UTF-8 dependency paths
//! fail closed rather than approximating Java's replacement-character decoder.

pub(crate) const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_FIELDS: usize = 1_000_000;
const MAX_DEPENDENCIES: usize = 100_000;
const MAX_DEPTH: usize = 100;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Dependencies<'a> {
    pub paths: Vec<&'a str>,
    pub requires_fallback: bool,
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, size: usize) -> Result<&'a [u8], String> {
        let end = self.offset.checked_add(size).ok_or("length overflow")?;
        let bytes = self.bytes.get(self.offset..end).ok_or("truncated field")?;
        self.offset = end;
        Ok(bytes)
    }

    fn varint(&mut self) -> Result<(u64, bool), String> {
        let mut result = 0u64;
        let mut nonzero = false;
        for shift in (0..70).step_by(7) {
            let byte = self.take(1)?[0];
            nonzero |= byte & 127 != 0;
            result |= u64::from(byte & 127).wrapping_shl(shift);
            if byte & 128 == 0 {
                return Ok((result, nonzero));
            }
        }
        Err("malformed varint (more than ten bytes)".into())
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        // CodedInputStream.readRawVarint32 and readBytes: lengths are signed
        // int32; overlong varints are accepted, negative lengths are not.
        let size = self.varint()?.0 as u32 as i32;
        if size < 0 {
            return Err("negative length".into());
        }
        self.take(size as usize)
    }

    fn tag(&mut self, budget: &mut usize) -> Result<Option<(u32, u8)>, String> {
        if self.offset == self.bytes.len() {
            return Ok(None);
        }
        *budget = budget.checked_sub(1).ok_or("field-count limit exceeded")?;
        let tag = self.varint()?.0 as u32;
        if tag >> 3 == 0 {
            return Err("invalid field number zero".into());
        }
        Ok(Some((tag >> 3, (tag & 7) as u8)))
    }

    fn skip(
        &mut self,
        number: u32,
        wire: u8,
        depth: usize,
        budget: &mut usize,
    ) -> Result<(), String> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                self.take(8)?;
            }
            2 => {
                self.bytes()?;
            }
            3 => {
                if depth >= MAX_DEPTH {
                    return Err("recursion limit exceeded".into());
                }
                loop {
                    let (nested, wire) = self.tag(budget)?.ok_or("unterminated group")?;
                    if wire == 4 {
                        if nested != number {
                            return Err("mismatched end-group".into());
                        }
                        break;
                    }
                    self.skip(nested, wire, depth + 1, budget)?;
                }
            }
            4 => return Err("unexpected end-group".into()),
            5 => {
                self.take(4)?;
            }
            _ => return Err("invalid wire type".into()),
        }
        Ok(())
    }
}

fn location(bytes: &[u8], budget: &mut usize) -> Result<(), String> {
    let mut reader = Reader::new(bytes);
    let mut path = false;
    while let Some((number, wire)) = reader.tag(budget)? {
        if (number, wire) == (1, 2) {
            reader.bytes()?;
            path = true;
        } else {
            reader.skip(number, wire, 2, budget)?;
        }
    }
    if !path {
        return Err("SourceLocation missing required path".into());
    }
    Ok(())
}

fn dependency<'a>(bytes: &'a [u8], budget: &mut usize) -> Result<&'a str, String> {
    let mut reader = Reader::new(bytes);
    let mut path = None;
    let mut kind = false;
    while let Some((number, wire)) = reader.tag(budget)? {
        match (number, wire) {
            (1, 2) => path = Some(reader.bytes()?),
            (2, 0) => {
                // Proto2 unknown enum values go to unknown fields. A prior
                // known value still satisfies initialization.
                kind |= (0..=3).contains(&(reader.varint()?.0 as u32 as i32));
            }
            (3, 2) => location(reader.bytes()?, budget)?,
            _ => reader.skip(number, wire, 1, budget)?,
        }
    }
    if !kind {
        return Err("Dependency missing required kind".into());
    }
    std::str::from_utf8(path.ok_or("Dependency missing required path")?)
        .map_err(|_| "invalid UTF-8 Dependency.path is not supported by the native profile".into())
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Dependencies<'_>, String> {
    if bytes.len() > MAX_BYTES {
        return Err("Java .jdeps exceeds 16 MiB limit".into());
    }
    let mut reader = Reader::new(bytes);
    let mut budget = MAX_FIELDS;
    let mut result = Dependencies {
        paths: Vec::new(),
        requires_fallback: false,
    };
    let read = || -> Result<Dependencies<'_>, String> {
        while let Some((number, wire)) = reader.tag(&mut budget)? {
            match (number, wire) {
                (1, 2) => {
                    if result.paths.len() >= MAX_DEPENDENCIES {
                        return Err("dependency-count limit exceeded".into());
                    }
                    result.paths.push(dependency(reader.bytes()?, &mut budget)?);
                }
                (5, 0) => result.requires_fallback = reader.varint()?.1,
                _ => reader.skip(number, wire, 0, &mut budget)?,
            }
        }
        Ok(result)
    };
    read().map_err(|error| format!("invalid Java .jdeps: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    // Captured against Deps.Dependencies.parseFrom in the pinned genuine
    // JavaBuilder jar (javabuilder/jdeps-parser-reference.txt).
    #[test]
    fn proto2_initialization_repeated_fields_enums_and_unknown_groups() {
        for (bytes, paths, fallback) in [
            ("", vec![], false),
            ("0a040a001000", vec![""], false),
            ("0a050a01611000", vec!["a"], false),
            ("0a070a016110001007", vec!["a"], false),
            ("0a080a01610a01621000", vec!["b"], false),
            ("0a070a016110001003", vec!["a"], false),
            ("28012800", vec![], false),
            ("2802", vec![], true),
            ("2a0101", vec![], false),
            ("0b0c333a01613b3c34", vec![], false),
            ("2880808080808080808002", vec![], true),
        ] {
            let bytes = hex(bytes);
            assert_eq!(
                parse(&bytes).unwrap(),
                Dependencies {
                    paths,
                    requires_fallback: fallback
                }
            );
        }
        for bytes in [
            "0a00",
            "0a050a01611007",
            "0a070a016110001a00",
            "00",
            "0c",
            "0a08",
            "280128",
            "0b14",
            "0e",
            "0f",
            "0980",
            "0aFFFFFFFF0f",
            "288080808080808080808000",
        ] {
            assert!(parse(&hex(bytes)).is_err(), "accepted {bytes}");
        }
        assert!(parse(&hex("0a050a01ff1000")).unwrap_err().contains("UTF-8"));
    }

    #[test]
    fn paths_are_exact_membership_values_not_filesystem_requests() {
        let mut bytes = Vec::new();
        for path in [
            "../x.jar",
            "/absolute.jar",
            "a/../b.jar",
            "",
            "same.jar",
            "same.jar",
        ] {
            bytes.extend([10, (path.len() + 4) as u8, 10, path.len() as u8]);
            bytes.extend(path.as_bytes());
            bytes.extend([16, 2]); // UNUSED is included too.
        }
        assert_eq!(
            parse(&bytes).unwrap().paths,
            [
                "../x.jar",
                "/absolute.jar",
                "a/../b.jar",
                "",
                "same.jar",
                "same.jar"
            ]
        );
    }

    #[test]
    fn decoder_bounds_size_count_and_recursion() {
        assert!(
            parse(&vec![0; MAX_BYTES + 1])
                .unwrap_err()
                .contains("16 MiB")
        );
        let mut deep = vec![11; MAX_DEPTH + 1];
        deep.extend(vec![12; MAX_DEPTH + 1]);
        assert!(parse(&deep).unwrap_err().contains("recursion"));
        let many = hex("0a040a001000").repeat(MAX_DEPENDENCIES + 1);
        assert!(parse(&many).unwrap_err().contains("dependency-count"));
        assert!(
            parse(&[16, 0].repeat(MAX_FIELDS + 1))
                .unwrap_err()
                .contains("field-count")
        );
    }
}
