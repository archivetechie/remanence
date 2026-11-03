//! Schema-directed deterministic CBOR shared by the tape and library log families.
//! Definite lengths, minimal integers and ordered mandatory maps are enforced
//! without allocating a generic value tree. Each family owns its record schema.

use super::MAX_FRAME_BODY;
use std::io::{self, Read, Write};

pub(crate) fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "noncanonical CBOR or invalid schema",
    )
}
pub(crate) trait Wire: Sized {
    fn put(&self, w: &mut dyn Write) -> io::Result<()>;
    fn get(r: &mut dyn Read) -> io::Result<Self>;
}
pub(crate) fn head(w: &mut dyn Write, major: u8, n: u64) -> io::Result<()> {
    let base = major << 5;
    match n {
        0..=23 => w.write_all(&[base | n as u8]),
        24..=255 => w.write_all(&[base | 24, n as u8]),
        256..=65535 => {
            w.write_all(&[base | 25])?;
            w.write_all(&(n as u16).to_be_bytes())
        }
        65536..=4294967295 => {
            w.write_all(&[base | 26])?;
            w.write_all(&(n as u32).to_be_bytes())
        }
        _ => {
            w.write_all(&[base | 27])?;
            w.write_all(&n.to_be_bytes())
        }
    }
}
pub(crate) fn read_head(r: &mut dyn Read) -> io::Result<(u8, u64)> {
    let mut b = [0];
    r.read_exact(&mut b)?;
    let a = b[0] & 31;
    let n = match a {
        0..=23 => a as u64,
        24..=27 => {
            let len = 1usize << (a - 24);
            let mut bytes = [0; 8];
            r.read_exact(&mut bytes[8 - len..])?;
            let n = u64::from_be_bytes(bytes);
            let min = [24, 256, 65536, 4294967296][(a - 24) as usize];
            if n < min {
                return Err(invalid());
            }
            n
        }
        _ => return Err(invalid()),
    };
    Ok((b[0] >> 5, n))
}
pub(crate) fn expect(r: &mut dyn Read, major: u8) -> io::Result<u64> {
    let (m, n) = read_head(r)?;
    if m != major {
        return Err(invalid());
    }
    Ok(n)
}
macro_rules! uint {
    ($($ty:ty),+) => {$ (impl Wire for $ty {
        fn put(&self,w:&mut dyn Write)->io::Result<()> { head(w,0,*self as u64) }
        fn get(r:&mut dyn Read)->io::Result<Self> { expect(r,0)?.try_into().map_err(|_|invalid()) }
    })+};
}
uint!(u8, u16, u32, u64);
impl Wire for bool {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        w.write_all(&[if *self { 0xf5 } else { 0xf4 }])
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        match read_head(r)? {
            (7, 20) => Ok(false),
            (7, 21) => Ok(true),
            _ => Err(invalid()),
        }
    }
}
impl<T: Wire> Wire for Option<T> {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            Some(v) => v.put(w),
            None => w.write_all(&[0xf6]),
        }
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        let mut b = [0];
        r.read_exact(&mut b)?;
        if b[0] == 0xf6 {
            Ok(None)
        } else {
            T::get(&mut b.as_slice().chain(r)).map(Some)
        }
    }
}
impl<const N: usize> Wire for [u8; N] {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 2, N as u64)?;
        w.write_all(self)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        if expect(r, 2)? != N as u64 {
            return Err(invalid());
        }
        let mut b = [0; N];
        r.read_exact(&mut b)?;
        Ok(b)
    }
}
impl Wire for Vec<u8> {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 2, self.len() as u64)?;
        w.write_all(self)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        bytes(r, 2)
    }
}
pub(crate) fn bytes(r: &mut dyn Read, major: u8) -> io::Result<Vec<u8>> {
    let len = expect(r, major)?;
    // A catalog spans frames, but each scalar field is bounded independently.
    // Refuse before allocating or consuming bytes from subsequent fields.
    if len > MAX_FRAME_BODY as u64 {
        return Err(invalid());
    }
    let mut out = Vec::new();
    let read = r.take(len).read_to_end(&mut out)?;
    if read as u64 != len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(out)
}
impl Wire for String {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 3, self.len() as u64)?;
        w.write_all(self.as_bytes())
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        String::from_utf8(bytes(r, 3)?).map_err(|_| invalid())
    }
}
macro_rules! enumeration {
    ($ty:ty; $($n:literal=>$variant:ident),+ $(,)?) => { impl Wire for $ty {
        fn put(&self,w:&mut dyn Write)->io::Result<()> { let n:u64=match self {$(Self::$variant=>$n),+}; n.put(w) }
        fn get(r:&mut dyn Read)->io::Result<Self> { match u64::get(r)? {$($n=>Ok(Self::$variant),)+_=>Err(invalid())} }
    }};
}

macro_rules! fields {
    ($ty:ty; $($field:ident),+ $(,)?) => { impl Wire for $ty {
        fn put(&self,w:&mut dyn Write)->io::Result<()> {
            let keys=[$(stringify!($field)),+]; head(w,5,keys.len() as u64)?;
            let mut key=0u64;
            $(key.put(w)?; self.$field.put(w)?; key+=1;)+ let _=key; Ok(())
        }
        fn get(r:&mut dyn Read)->io::Result<Self> {
            let keys=[$(stringify!($field)),+]; if expect(r,5)? != keys.len() as u64 {return Err(invalid());}
            let mut key=0u64;
            $(if u64::get(r)? != key { return Err(invalid()); } let $field=Wire::get(r)?; key+=1;)+ let _=key;
            Ok(Self {$($field),+})
        }
    }};
}

pub(crate) use {enumeration, fields};

/// Definite-length array with a schema-specific limit checked before allocation.
/// The tuple is crate-visible so schema code can inspect values without copying.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Library-log array primitive; no producer is connected in B2.
pub struct Bounded<T, const MAX: usize>(
    /// Elements, checked against the bound by both encoder and decoder.
    pub Vec<T>,
);

impl<T: Wire, const MAX: usize> Wire for Bounded<T, MAX> {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.0.len() > MAX {
            return Err(invalid());
        }
        head(w, 4, self.0.len() as u64)?;
        for value in &self.0 {
            value.put(w)?;
        }
        Ok(())
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        let count = expect(r, 4)?;
        if count > MAX as u64 {
            return Err(invalid());
        }
        // Grow only as elements are consumed, even when a valid count precedes EOF.
        let mut values = Vec::new();
        for _ in 0..count {
            values.push(T::get(r)?);
        }
        Ok(Self(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Oversize arrays are refused at their count, without reading an element.
    #[test]
    fn bounded_count_is_checked_before_elements() {
        struct HeaderOnly {
            bytes: &'static [u8],
        }
        impl Read for HeaderOnly {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                assert!(!self.bytes.is_empty(), "element read after hostile count");
                self.bytes.read(out)
            }
        }
        assert!(Bounded::<u64, 2>::get(&mut HeaderOnly { bytes: &[0x83] }).is_err());
        let value = Bounded::<u64, 2>(vec![1, 2, 3]);
        let mut bytes = Vec::new();
        assert!(value.put(&mut bytes).is_err());
        assert!(bytes.is_empty());
    }

    /// Minimal integers, definite arrays and exact typed values round-trip.
    #[test]
    fn bounded_arrays_preserve_canonical_values() {
        let value = Bounded::<Option<u64>, 3>(vec![Some(23), None, Some(256)]);
        let mut bytes = Vec::new();
        value.put(&mut bytes).unwrap();
        assert_eq!(bytes, [0x83, 0x17, 0xf6, 0x19, 0x01, 0x00]);
        assert_eq!(
            Bounded::<Option<u64>, 3>::get(&mut bytes.as_slice()).unwrap(),
            value
        );
        assert!(Bounded::<u64, 3>::get(&mut [0x9f, 0x01, 0xff].as_slice()).is_err());
        assert!(Bounded::<u64, 3>::get(&mut [0x81, 0x18, 0x01].as_slice()).is_err());
    }
}
