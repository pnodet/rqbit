use bencode::BencodeValue;
use bencode::bencode_serialize_to_writer;
use bencode::from_bytes;
use buffers::ByteBufT;
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use serde::Deserialize;
use ut_pex::UtPex;

use crate::MY_EXTENDED_UT_PEX;

use self::{handshake::ExtendedHandshake, ut_metadata::UtMetadata};

use super::MessageDeserializeError;

pub mod handshake;
pub mod ut_metadata;
pub mod ut_pex;

use super::MY_EXTENDED_UT_METADATA;

#[derive(Debug, Default)]
pub struct PeerExtendedMessageIds {
    pub ut_metadata: Option<u8>,
    pub ut_pex: Option<u8>,
}

#[derive(Debug)]
pub enum ExtendedMessage<ByteBuf: ByteBufT> {
    Handshake(ExtendedHandshake<ByteBuf>),
    UtMetadata(UtMetadata<ByteBuf>),
    UtPex(UtPex<ByteBuf>),
    Dyn(u8, BencodeValue<ByteBuf>),
}

impl<ByteBuf> CloneToOwned for ExtendedMessage<ByteBuf>
where
    ByteBuf: ByteBufT,
    <ByteBuf as CloneToOwned>::Target: ByteBufT,
{
    type Target = ExtendedMessage<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        match self {
            ExtendedMessage::Handshake(h) => {
                ExtendedMessage::Handshake(h.clone_to_owned(within_buffer))
            }
            ExtendedMessage::Dyn(u, d) => ExtendedMessage::Dyn(*u, d.clone_to_owned(within_buffer)),
            ExtendedMessage::UtMetadata(m) => {
                ExtendedMessage::UtMetadata(m.clone_to_owned(within_buffer))
            }
            ExtendedMessage::UtPex(m) => ExtendedMessage::UtPex(m.clone_to_owned(within_buffer)),
        }
    }
}

impl<ByteBuf: ByteBufT> ExtendedMessage<ByteBuf> {
    pub fn serialize(
        &self,
        out: &mut Vec<u8>,
        extended_handshake_ut_metadata: &dyn Fn() -> PeerExtendedMessageIds,
    ) -> anyhow::Result<()>
    where
        ByteBuf: AsRef<[u8]>,
    {
        match self {
            ExtendedMessage::Dyn(msg_id, v) => {
                out.push(*msg_id);
                bencode_serialize_to_writer(v, out)?;
            }
            ExtendedMessage::Handshake(h) => {
                out.push(0);
                bencode_serialize_to_writer(h, out)?;
            }
            ExtendedMessage::UtMetadata(u) => {
                let emsg_id = extended_handshake_ut_metadata()
                    .ut_metadata
                    .ok_or_else(|| {
                        anyhow::anyhow!("need peer's handshake to serialize ut_metadata")
                    })?;
                out.push(emsg_id);
                u.serialize(out);
            }
            ExtendedMessage::UtPex(m) => {
                let emsg_id = extended_handshake_ut_metadata().ut_pex.ok_or_else(|| {
                    anyhow::anyhow!(
                        "need peer's handshake to serialize ut_pex, or peer does't support ut_pex"
                    )
                })?;
                out.push(emsg_id);
                bencode_serialize_to_writer(m, out)?;
            }
        }
        Ok(())
    }

    pub fn deserialize<'a>(mut buf: &'a [u8]) -> Result<Self, MessageDeserializeError>
    where
        ByteBuf: Deserialize<'a> + From<&'a [u8]>,
    {
        let emsg_id = buf.first().copied().ok_or(MessageDeserializeError::Text(
            "cannot deserialize extended message: can't read first byte",
        ))?;

        buf = buf.get(1..).ok_or(MessageDeserializeError::Text(
            "cannot deserialize extended message: buffer empty",
        ))?;

        match emsg_id {
            0 => Ok(ExtendedMessage::Handshake(from_bytes(buf)?)),
            MY_EXTENDED_UT_METADATA => {
                Ok(ExtendedMessage::UtMetadata(UtMetadata::deserialize(buf)?))
            }
            MY_EXTENDED_UT_PEX => Ok(ExtendedMessage::UtPex(from_bytes(buf)?)),
            _ => Ok(ExtendedMessage::Dyn(emsg_id, from_bytes(buf)?)),
        }
    }
}
