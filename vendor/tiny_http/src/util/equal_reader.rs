use std::io::Read;
use std::io::Result as IoResult;
use std::io::{Error as IoError, ErrorKind};
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};

/// The most unread body bytes the destructor will read and throw away to
/// keep a keep-alive connection in sync (ralphus patch, see
/// `RALPHUS_PATCHES.md`). A longer remainder is left unread: the connection's
/// next read then fails to parse a request and the connection is closed,
/// which is the right outcome for a client that declared a body it never
/// meant to send.
pub(crate) const MAX_DRAIN: usize = 1024 * 1024;

/// A `Reader` that reads exactly the number of bytes from a sub-reader.
///
/// If the limit is reached, it returns EOF. If the limit is not reached
/// when the destructor is called, up to [`MAX_DRAIN`] remaining bytes are
/// read and thrown away through a fixed-size buffer.
pub struct EqualReader<R>
where
    R: Read,
{
    reader: R,
    size: usize,
    last_read_signal: Sender<IoResult<()>>,
}

impl<R> EqualReader<R>
where
    R: Read,
{
    pub fn new(reader: R, size: usize) -> (EqualReader<R>, Receiver<IoResult<()>>) {
        let (tx, rx) = channel();

        let r = EqualReader {
            reader,
            size,
            last_read_signal: tx,
        };

        (r, rx)
    }
}

impl<R> Read for EqualReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if self.size == 0 {
            return Ok(0);
        }

        let buf = if buf.len() < self.size {
            buf
        } else {
            &mut buf[..self.size]
        };

        match self.reader.read(buf) {
            Ok(len) => {
                self.size -= len;
                Ok(len)
            }
            err @ Err(_) => err,
        }
    }
}

impl<R> Drop for EqualReader<R>
where
    R: Read,
{
    fn drop(&mut self) {
        // ralphus patch: upstream allocated `vec![0; remaining]` here, so a
        // request declaring a huge `Content-Length` and sending a few bytes
        // aborted the whole process on allocation failure when it was
        // dropped.
        if self.size > MAX_DRAIN {
            self.last_read_signal
                .send(Err(IoError::new(
                    ErrorKind::InvalidData,
                    "unread request body too large to drain",
                )))
                .ok();
            return;
        }

        let mut buf = [0u8; 8192];
        while self.size > 0 {
            let len = self.size.min(buf.len());
            match self.reader.read(&mut buf[..len]) {
                Err(e) => {
                    self.last_read_signal.send(Err(e)).ok();
                    break;
                }
                Ok(0) => {
                    self.last_read_signal.send(Ok(())).ok();
                    break;
                }
                Ok(other) => {
                    self.size -= other;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EqualReader;
    use std::io::Read;

    #[test]
    fn test_limit() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut string = String::new();
            equal_reader.read_to_string(&mut string).unwrap();
            assert_eq!(string, "hello");
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }

    #[test]
    fn test_not_enough() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut vec = [0];
            equal_reader.read_exact(&mut vec).unwrap();
            assert_eq!(vec[0], b'h');
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }

    #[test]
    fn test_huge_declared_size_does_not_allocate_on_drop() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new(b"{}".to_vec());
        {
            let (mut equal_reader, rx) = EqualReader::new(org_reader.by_ref(), 99_999_999_999_999);
            let mut vec = [0];
            equal_reader.read_exact(&mut vec).unwrap();
            drop(equal_reader);
            assert!(rx.recv().unwrap().is_err());
        }
        // Nothing past what was read is consumed.
        assert_eq!(org_reader.position(), 1);
    }

    #[test]
    fn test_drain_within_limit_uses_bounded_buffer() {
        use std::io::Cursor;

        let body = vec![b'x'; super::MAX_DRAIN];
        let mut org_reader = Cursor::new(body);
        {
            let (_equal_reader, _) = EqualReader::new(org_reader.by_ref(), super::MAX_DRAIN);
        }
        assert_eq!(org_reader.position() as usize, super::MAX_DRAIN);
    }
}
