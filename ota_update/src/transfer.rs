// Copyright (c) 2026 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub const SOH: u8 = 0x01;
pub const FRAME_HEADER_LEN: usize = 10;
pub const MAX_PAYLOAD_LEN: usize = 2048;
pub const TRANSFER_INFO_FIXED_LEN: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Hello = 1,
    HelloAck = 2,
    Ping = 3,
    Pong = 4,
    DownloadRequest = 5,
    Update = 6,
    Ready = 7,
    Data = 8,
    Ack = 9,
    Nak = 10,
    TransferEnd = 11,
    Result = 12,
    Error = 13,
    Unchanged = 14,
    HttpPull = 15,
}

impl TryFrom<u8> for FrameType {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::HelloAck),
            3 => Ok(Self::Ping),
            4 => Ok(Self::Pong),
            5 => Ok(Self::DownloadRequest),
            6 => Ok(Self::Update),
            7 => Ok(Self::Ready),
            8 => Ok(Self::Data),
            9 => Ok(Self::Ack),
            10 => Ok(Self::Nak),
            11 => Ok(Self::TransferEnd),
            12 => Ok(Self::Result),
            13 => Ok(Self::Error),
            14 => Ok(Self::Unchanged),
            15 => Ok(Self::HttpPull),
            _ => Err(ProtocolError::UnknownFrameType),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    InvalidMarker,
    UnknownFrameType,
    SequenceMismatch,
    PayloadTooLarge,
    PayloadLengthMismatch,
    CrcMismatch,
    BufferTooSmall,
    InvalidFilename,
    InvalidMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_type: FrameType,
    pub sequence: u8,
    pub payload_len: u16,
    pub crc32: u32,
}

impl FrameHeader {
    pub fn new(
        frame_type: FrameType,
        sequence: u8,
        payload_len: usize,
        crc32: u32,
    ) -> Result<Self, ProtocolError> {
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge);
        }
        Ok(Self {
            frame_type,
            sequence,
            payload_len: payload_len as u16,
            crc32,
        })
    }

    pub fn encode(&self, output: &mut [u8; FRAME_HEADER_LEN]) {
        output[0] = SOH;
        output[1] = self.frame_type as u8;
        output[2] = self.sequence;
        output[3] = !self.sequence;
        output[4..6].copy_from_slice(&self.payload_len.to_le_bytes());
        output[6..10].copy_from_slice(&self.crc32.to_le_bytes());
    }

    pub fn decode(input: &[u8; FRAME_HEADER_LEN]) -> Result<Self, ProtocolError> {
        if input[0] != SOH {
            return Err(ProtocolError::InvalidMarker);
        }
        if input[3] != !input[2] {
            return Err(ProtocolError::SequenceMismatch);
        }
        let payload_len = u16::from_le_bytes([input[4], input[5]]);
        if usize::from(payload_len) > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge);
        }
        Ok(Self {
            frame_type: FrameType::try_from(input[1])?,
            sequence: input[2],
            payload_len,
            crc32: u32::from_le_bytes([input[6], input[7], input[8], input[9]]),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub header: FrameHeader,
    pub payload: &'a [u8],
}

pub fn encode_frame(
    frame_type: FrameType,
    sequence: u8,
    payload: &[u8],
    output: &mut [u8],
) -> Result<usize, ProtocolError> {
    let frame_len = FRAME_HEADER_LEN
        .checked_add(payload.len())
        .ok_or(ProtocolError::PayloadTooLarge)?;
    if output.len() < frame_len {
        return Err(ProtocolError::BufferTooSmall);
    }
    let header = FrameHeader::new(frame_type, sequence, payload.len(), crc32(payload))?;
    let mut encoded_header = [0u8; FRAME_HEADER_LEN];
    header.encode(&mut encoded_header);
    output[..FRAME_HEADER_LEN].copy_from_slice(&encoded_header);
    output[FRAME_HEADER_LEN..frame_len].copy_from_slice(payload);
    Ok(frame_len)
}

pub fn decode_frame_raw_with_payload_len(
    input: &[u8],
    expected_payload_len: usize,
) -> Result<(FrameHeader, &[u8], bool), ProtocolError> {
    let frame_len = FRAME_HEADER_LEN
        .checked_add(expected_payload_len)
        .ok_or(ProtocolError::PayloadTooLarge)?;
    if input.len() != frame_len {
        return Err(ProtocolError::PayloadLengthMismatch);
    }
    let header_bytes: &[u8; FRAME_HEADER_LEN] = input[..FRAME_HEADER_LEN]
        .try_into()
        .map_err(|_| ProtocolError::PayloadLengthMismatch)?;
    let header = FrameHeader::decode(header_bytes)?;
    if usize::from(header.payload_len) != expected_payload_len {
        return Err(ProtocolError::PayloadLengthMismatch);
    }
    let payload = &input[FRAME_HEADER_LEN..];
    let crc_valid = crc32(payload) == header.crc32;
    Ok((header, payload, crc_valid))
}

pub fn decode_frame(input: &[u8]) -> Result<Frame<'_>, ProtocolError> {
    let expected_payload_len = input
        .len()
        .checked_sub(FRAME_HEADER_LEN)
        .ok_or(ProtocolError::PayloadLengthMismatch)?;
    let (header, payload, crc_valid) =
        decode_frame_raw_with_payload_len(input, expected_payload_len)?;
    if !crc_valid {
        return Err(ProtocolError::CrcMismatch);
    }
    Ok(Frame { header, payload })
}

pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut crc = crc;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

pub fn crc32(data: &[u8]) -> u32 {
    !crc32_update(0xFFFF_FFFF, data)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferInfo<'a> {
    pub filename: &'a str,
    pub size: u32,
    pub crc32: u32,
}

impl<'a> TransferInfo<'a> {
    pub fn encoded_len(&self) -> Result<usize, ProtocolError> {
        validate_filename(self.filename)?;
        if self.size == 0 || self.filename.len() > u8::MAX as usize {
            return Err(ProtocolError::InvalidMetadata);
        }
        let len = TRANSFER_INFO_FIXED_LEN + self.filename.len();
        if len > MAX_PAYLOAD_LEN {
            return Err(ProtocolError::PayloadTooLarge);
        }
        Ok(len)
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize, ProtocolError> {
        let encoded_len = self.encoded_len()?;
        if output.len() < encoded_len {
            return Err(ProtocolError::BufferTooSmall);
        }
        output[0] = self.filename.len() as u8;
        output[1..5].copy_from_slice(&self.size.to_le_bytes());
        output[5..9].copy_from_slice(&self.crc32.to_le_bytes());
        output[TRANSFER_INFO_FIXED_LEN..encoded_len].copy_from_slice(self.filename.as_bytes());
        Ok(encoded_len)
    }

    pub fn decode(input: &'a [u8]) -> Result<Self, ProtocolError> {
        if input.len() < TRANSFER_INFO_FIXED_LEN {
            return Err(ProtocolError::InvalidMetadata);
        }
        let name_len = usize::from(input[0]);
        if name_len == 0 || input.len() != TRANSFER_INFO_FIXED_LEN + name_len {
            return Err(ProtocolError::InvalidMetadata);
        }
        let filename = core::str::from_utf8(&input[TRANSFER_INFO_FIXED_LEN..])
            .map_err(|_| ProtocolError::InvalidFilename)?;
        validate_filename(filename)?;
        let size = u32::from_le_bytes([input[1], input[2], input[3], input[4]]);
        if size == 0 {
            return Err(ProtocolError::InvalidMetadata);
        }
        Ok(Self {
            filename,
            size,
            crc32: u32::from_le_bytes([input[5], input[6], input[7], input[8]]),
        })
    }
}

pub fn validate_filename(filename: &str) -> Result<(), ProtocolError> {
    let bytes = filename.as_bytes();
    if bytes.is_empty()
        || filename == "."
        || filename == ".."
        || bytes
            .iter()
            .any(|byte| *byte == b'/' || *byte == b'\\' || *byte == 0)
    {
        return Err(ProtocolError::InvalidFilename);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_frame_type_codes_round_trip() {
        let frame_types = [
            FrameType::Hello,
            FrameType::HelloAck,
            FrameType::Ping,
            FrameType::Pong,
            FrameType::DownloadRequest,
            FrameType::Update,
            FrameType::Ready,
            FrameType::Data,
            FrameType::Ack,
            FrameType::Nak,
            FrameType::TransferEnd,
            FrameType::Result,
            FrameType::Error,
            FrameType::Unchanged,
            FrameType::HttpPull,
        ];
        for frame_type in frame_types {
            assert_eq!(FrameType::try_from(frame_type as u8), Ok(frame_type));
        }
        assert_eq!(
            FrameType::try_from(0xff),
            Err(ProtocolError::UnknownFrameType)
        );
    }

    #[test]
    fn encodes_and_decodes_fixed_header() {
        let header = FrameHeader::new(FrameType::Data, 0x34, 512, 0x1234_5678).unwrap();
        let mut encoded = [0u8; FRAME_HEADER_LEN];
        header.encode(&mut encoded);
        assert_eq!(encoded[0], SOH);
        assert_eq!(encoded[2], 0x34);
        assert_eq!(encoded[3], !0x34);
        assert_eq!(FrameHeader::decode(&encoded), Ok(header));
    }

    #[test]
    fn rejects_bad_sequence_complement() {
        let mut encoded = [0u8; FRAME_HEADER_LEN];
        FrameHeader::new(FrameType::Ack, 7, 0, crc32(&[]))
            .unwrap()
            .encode(&mut encoded);
        encoded[3] ^= 1;
        assert_eq!(
            FrameHeader::decode(&encoded),
            Err(ProtocolError::SequenceMismatch)
        );
    }

    #[test]
    fn rejects_data_payload_larger_than_max() {
        assert_eq!(
            FrameHeader::new(FrameType::Data, 0, MAX_PAYLOAD_LEN + 1, 0),
            Err(ProtocolError::PayloadTooLarge)
        );
    }

    #[test]
    fn frame_round_trip_validates_length_and_crc() {
        let payload = b"123456789";
        let mut encoded = [0u8; FRAME_HEADER_LEN + 9];
        let written = encode_frame(FrameType::Data, 3, payload, &mut encoded).unwrap();
        let frame = decode_frame(&encoded[..written]).unwrap();
        assert_eq!(frame.header.frame_type, FrameType::Data);
        assert_eq!(frame.header.sequence, 3);
        assert_eq!(frame.payload, payload);
        assert_eq!(frame.header.crc32, 0xCBF4_3926);
        encoded[FRAME_HEADER_LEN] ^= 1;
        assert_eq!(decode_frame(&encoded), Err(ProtocolError::CrcMismatch));
        assert_eq!(
            decode_frame(&encoded[..encoded.len() - 1]),
            Err(ProtocolError::PayloadLengthMismatch)
        );
    }

    #[test]
    fn contiguous_frame_requires_expected_payload_length() {
        let payload = b"abc";
        let mut encoded = [0u8; FRAME_HEADER_LEN + 3];
        let written = encode_frame(FrameType::Data, 4, payload, &mut encoded).unwrap();
        let (header, decoded, crc_valid) =
            decode_frame_raw_with_payload_len(&encoded[..written], payload.len()).unwrap();
        assert_eq!(header.frame_type, FrameType::Data);
        assert_eq!(decoded, payload);
        assert!(crc_valid);
        assert_eq!(
            decode_frame_raw_with_payload_len(&encoded[..written], payload.len() - 1),
            Err(ProtocolError::PayloadLengthMismatch)
        );
    }

    #[test]
    fn transfer_info_round_trip_uses_caller_buffer() {
        let info = TransferInfo {
            filename: "payload.elf",
            size: 1_048_576,
            crc32: 0xCBF4_3926,
        };
        let mut encoded = [0u8; 64];
        let written = info.encode(&mut encoded).unwrap();
        assert_eq!(TransferInfo::decode(&encoded[..written]), Ok(info));
    }

    #[test]
    fn transfer_info_rejects_invalid_metadata() {
        let empty_name = TransferInfo {
            filename: "",
            size: 1,
            crc32: 0,
        };
        assert_eq!(
            empty_name.encoded_len(),
            Err(ProtocolError::InvalidFilename)
        );
        let mut truncated = [0u8; TRANSFER_INFO_FIXED_LEN];
        truncated[0] = 1;
        assert_eq!(
            TransferInfo::decode(&truncated),
            Err(ProtocolError::InvalidMetadata)
        );
    }
}
