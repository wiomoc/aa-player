use byteorder::{BigEndian, ByteOrder};

#[derive(Debug, Clone, Copy)]
pub(crate) enum AAPFrameFragmmentation {
    Continuation = 0,
    First = 1,
    Last = 2,
    Unfragmented = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AAPFrameType {
    ChannelSpecific = 0,
    Control = 1,
}

#[derive(Debug)]
pub(crate) struct AAPFrame {
    pub channel_id: u8,
    pub frag_info: AAPFrameFragmmentation,
    pub r#type: AAPFrameType,
    pub encrypted: bool,
    pub total_payload_length: usize,
    pub payload: Vec<u8>,
}

pub(crate) trait AsyncReader {
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), std::io::Error>;
}

pub(crate) trait AsyncWriter {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), std::io::Error>;
}

pub(crate) trait FrameDecoder<R: AsyncReader> {
    type Item;

    async fn read_frame(&mut self, reader: &mut R) -> Result<Self::Item, std::io::Error>;
}

pub(crate) trait FrameEncoder<W: AsyncWriter> {
    type Item;

    async fn write_frame(&mut self, item: Self::Item, writer: &mut W)
    -> Result<(), std::io::Error>;
}

pub(crate) struct AAPFrameCodec;

impl AAPFrameCodec {
    const LONG_PAYLOAD_LENGTH_MASK: u8 = 0x03;
    const FRAGMENTATION_MASK: u8 = 0x03;
    const TYPE_BIT_OFFSET: u8 = 2;
    const TYPE_MASK: u8 = (1 << Self::TYPE_BIT_OFFSET);
    const ENCRYPTED_PAYLOAD_BIT_OFFSET: u8 = 3;
    const ENCRYPTED_PAYLOAD_MASK: u8 = (1 << Self::ENCRYPTED_PAYLOAD_BIT_OFFSET);

    pub(crate) fn new() -> Self {
        Self
    }
}

impl<R: AsyncReader> FrameDecoder<R> for AAPFrameCodec {
    type Item = AAPFrame;

    async fn read_frame(&mut self, reader: &mut R) -> Result<Self::Item, std::io::Error> {
        let mut header = [0u8; 4];

        reader.read_exact(&mut header).await?;

        let channel_id = header[0];
        let flags = header[1];

        let frag_info = match flags & AAPFrameCodec::FRAGMENTATION_MASK {
            0 => AAPFrameFragmmentation::Continuation,
            1 => AAPFrameFragmmentation::First,
            2 => AAPFrameFragmmentation::Last,
            3 => AAPFrameFragmmentation::Unfragmented,
            _ => panic!("invalid frame frag"),
        };

        let fragment_payload_length = BigEndian::read_u16(&header[2..]) as usize;

        let total_payload_length = if matches!(frag_info, AAPFrameFragmmentation::First) {
            let mut length_buffer = [header[2], header[3], 0, 0];
            reader.read_exact(&mut length_buffer[2..]).await?;

            BigEndian::read_u32(&length_buffer) as usize
        } else {
            fragment_payload_length
        };

        let encrypted = (flags & AAPFrameCodec::ENCRYPTED_PAYLOAD_MASK)
            == AAPFrameCodec::ENCRYPTED_PAYLOAD_MASK;

        let r#type = if flags & AAPFrameCodec::TYPE_MASK == AAPFrameCodec::TYPE_MASK {
            AAPFrameType::Control
        } else {
            AAPFrameType::ChannelSpecific
        };

        let mut payload = vec![0u8; fragment_payload_length];
        reader.read_exact(&mut payload).await?;

        Ok(AAPFrame {
            channel_id,
            frag_info,
            r#type,
            encrypted,
            total_payload_length,
            payload,
        })
    }
}

impl<W: AsyncWriter> FrameEncoder<W> for AAPFrameCodec {
    type Item = AAPFrame;

    async fn write_frame(
        &mut self,
        item: Self::Item,
        writer: &mut W,
    ) -> Result<(), std::io::Error> {
        let mut flags = 0;
        if matches!(item.r#type, AAPFrameType::Control) {
            flags |= AAPFrameCodec::TYPE_MASK;
        };
        if item.encrypted {
            flags |= AAPFrameCodec::ENCRYPTED_PAYLOAD_MASK;
        }
        flags |= item.frag_info as u8;

        let mut header = [0u8; 8];
        header[0] = item.channel_id;

        BigEndian::write_u16(&mut header[2..], item.payload.len() as u16);

        let header = if matches!(item.frag_info, AAPFrameFragmmentation::First) {
            flags |= AAPFrameCodec::LONG_PAYLOAD_LENGTH_MASK;
            BigEndian::write_u32(&mut header[4..], item.total_payload_length as u32);
            &mut header
        } else {
            &mut header[..4]
        };

        header[1] = flags;

        writer.write_all(header).await?;
        writer.write_all(&item.payload).await?;
        Ok(())
    }
}
