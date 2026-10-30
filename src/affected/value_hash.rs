//! A serde serializer that feeds a value's data into a SHA-256, every string
//! and byte string normalized (`super::normalized`: the workspace root and
//! repository store tree identities replaced by tokens). Each datum is
//! framed with a tag and its own length *after* normalization, so a value
//! planned in a checkout at a path of another length (a second worktree, a
//! working copy planned in place) hashes alike — a serialized byte stream's
//! length prefixes would keep the root's length.

use super::{frame, normalized};
use serde::ser::{self, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub(super) struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl ser::Error for Error {
    fn custom<T: std::fmt::Display>(message: T) -> Self {
        Error(message.to_string())
    }
}

pub(super) struct Hasher<'a> {
    pub hash: &'a mut Sha256,
    pub root: &'a [u8],
}

impl Hasher<'_> {
    fn tag(&mut self, tag: &[u8]) {
        frame(self.hash, tag);
    }

    fn bytes(&mut self, tag: &[u8], bytes: &[u8]) {
        self.tag(tag);
        frame(self.hash, &normalized(bytes, self.root));
    }

    fn len(&mut self, len: Option<usize>) {
        // Sequences and maps of unknown length are closed by an end tag.
        self.hash
            .update((len.map_or(u64::MAX, |len| len as u64)).to_le_bytes());
    }
}

type Result<T = ()> = std::result::Result<T, Error>;

impl<'a, 'b> ser::Serializer for &'a mut Hasher<'b> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_bool(self, value: bool) -> Result {
        self.tag(if value { b"T" } else { b"F" });
        Ok(())
    }
    fn serialize_i8(self, value: i8) -> Result {
        self.serialize_i64(value.into())
    }
    fn serialize_i16(self, value: i16) -> Result {
        self.serialize_i64(value.into())
    }
    fn serialize_i32(self, value: i32) -> Result {
        self.serialize_i64(value.into())
    }
    fn serialize_i64(self, value: i64) -> Result {
        self.tag(b"i");
        self.hash.update(value.to_le_bytes());
        Ok(())
    }
    fn serialize_u8(self, value: u8) -> Result {
        self.serialize_u64(value.into())
    }
    fn serialize_u16(self, value: u16) -> Result {
        self.serialize_u64(value.into())
    }
    fn serialize_u32(self, value: u32) -> Result {
        self.serialize_u64(value.into())
    }
    fn serialize_u64(self, value: u64) -> Result {
        self.tag(b"u");
        self.hash.update(value.to_le_bytes());
        Ok(())
    }
    fn serialize_f32(self, value: f32) -> Result {
        self.serialize_f64(value.into())
    }
    fn serialize_f64(self, value: f64) -> Result {
        self.tag(b"d");
        self.hash.update(value.to_bits().to_le_bytes());
        Ok(())
    }
    fn serialize_char(self, value: char) -> Result {
        let mut buffer = [0; 4];
        self.serialize_str(value.encode_utf8(&mut buffer))
    }
    fn serialize_str(self, value: &str) -> Result {
        self.bytes(b"s", value.as_bytes());
        Ok(())
    }
    fn serialize_bytes(self, value: &[u8]) -> Result {
        self.bytes(b"b", value);
        Ok(())
    }
    fn serialize_none(self) -> Result {
        self.tag(b"N");
        Ok(())
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result {
        self.tag(b"S");
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result {
        self.tag(b"U");
        Ok(())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result {
        self.serialize_unit()
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
    ) -> Result {
        self.tag(b"v");
        self.hash.update(index.to_le_bytes());
        Ok(())
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result {
        self.tag(b"V");
        self.hash.update(index.to_le_bytes());
        value.serialize(self)
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self> {
        self.tag(b"[");
        self.len(len);
        Ok(self)
    }
    fn serialize_tuple(self, len: usize) -> Result<Self> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<Self> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self> {
        self.tag(b"V");
        self.hash.update(index.to_le_bytes());
        self.serialize_seq(Some(len))
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self> {
        self.tag(b"{");
        self.len(len);
        Ok(self)
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<Self> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Self> {
        self.tag(b"V");
        self.hash.update(index.to_le_bytes());
        self.serialize_map(Some(len))
    }
}

impl ser::SerializeSeq for &mut Hasher<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result {
        self.tag(b"]");
        Ok(())
    }
}

impl ser::SerializeTuple for &mut Hasher<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result {
        self.tag(b"]");
        Ok(())
    }
}

impl ser::SerializeTupleStruct for &mut Hasher<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result {
        self.tag(b"]");
        Ok(())
    }
}

impl ser::SerializeTupleVariant for &mut Hasher<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result {
        self.tag(b"]");
        Ok(())
    }
}

impl ser::SerializeMap for &mut Hasher<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result {
        self.tag(b"k");
        key.serialize(&mut **self)
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result {
        self.tag(b"}");
        Ok(())
    }
}

impl ser::SerializeStruct for &mut Hasher<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, name: &'static str, value: &T) -> Result {
        self.tag(name.as_bytes());
        value.serialize(&mut **self)
    }
    fn skip_field(&mut self, name: &'static str) -> Result {
        self.tag(b"-");
        self.tag(name.as_bytes());
        Ok(())
    }
    fn end(self) -> Result {
        self.tag(b"}");
        Ok(())
    }
}

impl ser::SerializeStructVariant for &mut Hasher<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, name: &'static str, value: &T) -> Result {
        self.tag(name.as_bytes());
        value.serialize(&mut **self)
    }
    fn end(self) -> Result {
        self.tag(b"}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest<T: Serialize>(value: &T, root: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        value
            .serialize(&mut Hasher {
                hash: &mut hash,
                root,
            })
            .unwrap();
        hash.finalize().into()
    }

    #[derive(serde::Serialize)]
    struct Action {
        argv: Vec<String>,
        #[serde(with = "serde_bytes_path")]
        input: std::path::PathBuf,
        env: std::collections::BTreeMap<String, String>,
    }

    mod serde_bytes_path {
        use std::os::unix::ffi::OsStrExt;
        pub fn serialize<S: serde::Serializer>(
            path: &std::path::Path,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            serializer.serialize_bytes(path.as_os_str().as_bytes())
        }
    }

    fn action(root: &str, name: &str) -> Action {
        Action {
            argv: vec!["cat".into(), format!("{root}/{name}")],
            input: format!("{root}/{name}").into(),
            env: [("PWD".to_owned(), root.to_owned())].into_iter().collect(),
        }
    }

    #[test]
    fn roots_of_different_lengths_hash_alike() {
        let short = digest(&action("/w/a", "x.txt"), b"/w/a");
        let long = digest(&action("/w/a-longer", "x.txt"), b"/w/a-longer");
        assert_eq!(short, long);
        assert_ne!(
            short,
            digest(&action("/w/a-longer", "y.txt"), b"/w/a-longer")
        );
        // Not the root: kept.
        assert_ne!(short, digest(&action("/w/b", "x.txt"), b"/w/a"));
    }
}
