// Copyright 2026 Ryan Moore
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

/// A bounds-checked cursor over untrusted bytes. Every read returns `None`
/// instead of panicking when the input is short.
pub(super) struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }

    pub(super) fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.remaining.len() < len {
            return None;
        }
        let (head, tail) = self.remaining.split_at(len);
        self.remaining = tail;
        Some(head)
    }

    pub(super) fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }

    pub(super) fn u16(&mut self) -> Option<u16> {
        self.bytes(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    /// A vector with a one-byte length prefix (TLS `opaque x<0..2^8-1>`).
    pub(super) fn vec8(&mut self) -> Option<&'a [u8]> {
        let len = self.u8()?;
        self.bytes(usize::from(len))
    }

    /// A vector with a two-byte length prefix (TLS `opaque x<0..2^16-1>`).
    pub(super) fn vec16(&mut self) -> Option<&'a [u8]> {
        let len = self.u16()?;
        self.bytes(usize::from(len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_reads_return_none_without_consuming_past_end() {
        let mut reader = Reader::new(&[0x00, 0x05, 0xaa]);
        assert_eq!(reader.vec16(), None);
        assert_eq!(Reader::new(&[]).u8(), None);
        assert_eq!(Reader::new(&[1]).u16(), None);
    }

    #[test]
    fn length_prefixed_vectors_split_correctly() {
        let mut reader = Reader::new(&[0x02, 0xaa, 0xbb, 0x00, 0x01, 0xcc]);
        assert_eq!(reader.vec8(), Some(&[0xaa, 0xbb][..]));
        assert_eq!(reader.vec16(), Some(&[0xcc][..]));
        assert!(reader.is_empty());
    }
}
