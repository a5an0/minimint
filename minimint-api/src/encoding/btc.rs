use crate::encoding::{Decodable, DecodeError, Encodable};
use std::io::Error;

macro_rules! impl_encode_decode_bridge {
    ($btc_type:ty) => {
        impl crate::encoding::Encodable for $btc_type {
            fn consensus_encode<W: std::io::Write>(
                &self,
                writer: W,
            ) -> Result<usize, std::io::Error> {
                bitcoin::consensus::Encodable::consensus_encode(self, writer)
            }
        }

        impl crate::encoding::Decodable for $btc_type {
            fn consensus_decode<D: std::io::Read>(
                d: D,
            ) -> Result<Self, crate::encoding::DecodeError> {
                bitcoin::consensus::Decodable::consensus_decode(d)
                    .map_err(crate::encoding::DecodeError::from_err)
            }
        }
    };
}

impl_encode_decode_bridge!(bitcoin::BlockHeader);
impl_encode_decode_bridge!(bitcoin::BlockHash);
impl_encode_decode_bridge!(bitcoin::OutPoint);
impl_encode_decode_bridge!(bitcoin::Script);
impl_encode_decode_bridge!(bitcoin::Transaction);
impl_encode_decode_bridge!(bitcoin::Txid);
impl_encode_decode_bridge!(bitcoin::util::merkleblock::PartialMerkleTree);
impl_encode_decode_bridge!(bitcoin::util::psbt::PartiallySignedTransaction);

impl Encodable for bitcoin::Amount {
    fn consensus_encode<W: std::io::Write>(&self, writer: W) -> Result<usize, std::io::Error> {
        self.as_sat().consensus_encode(writer)
    }
}

impl Decodable for bitcoin::Amount {
    fn consensus_decode<D: std::io::Read>(d: D) -> Result<Self, DecodeError> {
        Ok(bitcoin::Amount::from_sat(u64::consensus_decode(d)?))
    }
}

// FIXME: find a proper binary encoding that still includes the network
impl Encodable for bitcoin::Address {
    fn consensus_encode<W: std::io::Write>(&self, writer: W) -> Result<usize, Error> {
        self.to_string().as_bytes().consensus_encode(writer)
    }
}

impl Decodable for bitcoin::Address {
    fn consensus_decode<D: std::io::Read>(d: D) -> Result<Self, DecodeError> {
        let bytes = Vec::<u8>::consensus_decode(d)?;
        String::from_utf8(bytes)
            .map_err(DecodeError::from_err)?
            .parse()
            .map_err(DecodeError::from_err)
    }
}
