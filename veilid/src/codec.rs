use bytes::{BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid message format")]
    InvalidFormat,
}

pub struct Codec;

impl Decoder for Codec {
    type Item = String;
    type Error = Error;

    fn decode(
        &mut self,
        src: &mut BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        // Veilid messages are newline-delimited strings
        // This is a placeholder implementation
        if let Some(i) = src.iter().position(|&b| b == b'\n') {
            let line = src.split_to(i + 1);
            let line = &line[..line.len() - 1];
            let s = String::from_utf8(line.to_vec())
                .map_err(|_| Error::InvalidFormat)?;
            Ok(Some(s))
        } else {
            Ok(None)
        }
    }
}

impl Encoder<String> for Codec {
    type Error = Error;

    fn encode(
        &mut self,
        item: String,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        dst.reserve(item.len() + 1);
        dst.put_slice(item.as_bytes());
        dst.put_u8(b'\n');
        Ok(())
    }
}
