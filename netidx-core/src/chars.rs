use crate::pack::{Pack, PackError};
use bytes::{Bytes, Buf, BufMut};
use std::{
    cmp::{Eq, Ord, Ordering, PartialEq, PartialOrd},
    convert::AsRef,
    borrow::Borrow,
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    str,
};

/// This is a thin wrapper around a Bytes that guarantees that it's contents are
/// well formed unicode.
#[derive(Clone, Serialize, Deserialize)]
pub struct Chars(Bytes);

impl Chars {
    pub fn new() -> Chars {
        Chars(Bytes::new())
    }

    pub fn from_bytes(bytes: Bytes) -> Result<Chars, str::Utf8Error> {
        str::from_utf8(&bytes)?;
        Ok(Chars(bytes))
    }

    pub unsafe fn from_bytes_unchecked(bytes: Bytes) -> Chars {
        Chars(bytes)
    }
    
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn bytes(&self) -> &[u8] {
        &*self.0
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl PartialEq for Chars {
    fn eq(&self, other: &Chars) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl Eq for Chars {}

impl PartialOrd for Chars {
    fn partial_cmp(&self, other: &Chars) -> Option<Ordering> {
        self.as_ref().partial_cmp(other.as_ref())
    }
}

impl Ord for Chars {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_ref().cmp(other.as_ref())
    }
}

impl Hash for Chars {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state)
    }
}

impl AsRef<str> for Chars {
    fn as_ref(&self) -> &str {
        unsafe { str::from_utf8_unchecked(&self.0) }
    }
}

impl Pack for Chars {
    fn encoded_len(&self) -> usize {
        Pack::encoded_len(&self.0)
    }

    fn encode(&self, buf: &mut impl BufMut) -> Result<(), PackError> {
        Pack::encode(&self.0, buf)
    }

    fn decode(buf: &mut impl Buf) -> Result<Self, PackError> {
        match Chars::from_bytes(<Bytes as Pack>::decode(buf)?) {
            Ok(c) => Ok(c),
            Err(_) => Err(PackError::InvalidFormat),
        }
    }
}

impl From<&'static str> for Chars {
    fn from(src: &'static str) -> Chars {
        Chars(Bytes::from(src.as_bytes()))
    }
}

impl From<String> for Chars {
    fn from(src: String) -> Chars {
        Chars(Bytes::from(src))
    }
}

impl Into<String> for &Chars {
    fn into(self) -> String {
        self.as_ref().into()
    }
}

impl Into<String> for Chars {
    fn into(self) -> String {
        self.as_ref().into()
    }
}

impl Deref for Chars {
    type Target = str;

    fn deref(&self) -> &str {
        unsafe { str::from_utf8_unchecked(&*self.0) }
    }
}

impl Borrow<str> for Chars {
    fn borrow(&self) -> &str {
        &*self
    }
}

impl fmt::Display for Chars {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl fmt::Debug for Chars {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}
