use bytes::Buf;
use bytes::Bytes;

// TODO: some tests
#[derive(Default)]
pub struct WriteBuffer {
    data: Vec<u8>,
    position: usize, // must be `<= data.len()`
}

impl Buf for WriteBuffer {
    /// Size of data in the buffer
    fn remaining(&self) -> usize {
        debug_assert!(self.position <= self.data.len());
        self.data.len() - self.position
    }

    fn bytes(&self) -> &[u8] {
        &self.data[self.position..]
    }

    fn advance(&mut self, cnt: usize) {
        assert!(cnt <= self.remaining());
        self.position += cnt;
    }
}

impl WriteBuffer {
    pub fn new() -> WriteBuffer {
        Default::default()
    }

    pub fn reserve(&mut self, additional: usize) {
        if self.remaining() >= additional {
            return;
        }
        self.compact();
        self.data.reserve(additional);
    }

    fn compact(&mut self) {
        self.data.drain(..self.position);
        self.position = 0;
    }

    pub fn extend_from_slice(&mut self, data: &[u8]) {
        // Could do something smarter
        self.reserve(data.len());
        self.data.extend_from_slice(data);
    }

    pub fn extend_from_vec(&mut self, data: Vec<u8>) {
        self.extend_from_slice(&data);
    }

    pub fn extend_from_bytes(&mut self, data: Bytes) {
        self.extend_from_slice(&data);
    }

    pub fn extend_from_bytes_ref(&mut self, data: &Bytes) {
        self.extend_from_slice(&*data);
    }

    pub fn extend_from_iter(&mut self, iter: impl Iterator<Item = u8>) {
        // Could do something smarter
        self.compact();
        self.data.extend(iter);
    }

    pub fn tail_vec(&mut self) -> WriteBufferTailVec {
        WriteBufferTailVec {
            data: &mut self.data,
            position: &mut self.position,
        }
    }
}

impl Into<Vec<u8>> for WriteBuffer {
    fn into(mut self) -> Vec<u8> {
        self.compact();
        self.data
    }
}

impl Into<Bytes> for WriteBuffer {
    fn into(self) -> Bytes {
        Bytes::from(Into::<Vec<u8>>::into(self))
    }
}

pub struct WriteBufferTailVec<'a> {
    data: &'a mut Vec<u8>,
    position: &'a mut usize,
}

impl<'a> WriteBufferTailVec<'a> {
    /// Size of data in the buffer
    pub fn remaining(&self) -> usize {
        debug_assert!(*self.position <= self.data.len());
        self.data.len() - *self.position
    }

    /// Pos is relative to "data"
    pub fn patch_buf(&mut self, pos: usize, data: &[u8]) {
        let patch_pos = *self.position + pos;
        (&mut self.data[patch_pos..patch_pos + data.len()]).copy_from_slice(data);
    }

    pub fn extend_from_slice(&mut self, data: &[u8]) {
        // Could do something smarter
        self.reserve(data.len());
        self.data.extend_from_slice(data);
    }

    pub fn reserve(&mut self, additional: usize) {
        if self.remaining() >= additional {
            return;
        }
        self.compact();
        self.data.reserve(additional);
    }

    pub fn compact(&mut self) {
        self.data.drain(..*self.position);
        *self.position = 0;
    }
}

impl<'a> Drop for WriteBufferTailVec<'a> {
    fn drop(&mut self) {}
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn remaining() {
        let mut buf = WriteBuffer::new();
        buf.extend_from_slice(b"abcd");
        assert_eq!(4, buf.remaining());

        assert_eq!(b'a', buf.get_u8());
        assert_eq!(b'b', buf.get_u8());
        assert_eq!(2, buf.remaining());

        buf.extend_from_slice(b"ef");
        assert_eq!(b'c', buf.get_u8());
        assert_eq!(b'd', buf.get_u8());
        assert_eq!(b'e', buf.get_u8());
        assert_eq!(b'f', buf.get_u8());
        assert_eq!(0, buf.remaining());
    }
}
