//! Generated protobuf types for Jito gRPC

pub mod auth {
    tonic::include_proto!("auth");
}

pub mod bundle {
    tonic::include_proto!("bundle");
}

pub mod packet {
    tonic::include_proto!("packet");
}

pub mod searcher {
    tonic::include_proto!("searcher");
}

pub mod shared {
    tonic::include_proto!("shared");
}

use solana_sdk::transaction::VersionedTransaction;

/// Convert a VersionedTransaction to a protobuf Packet
pub fn proto_packet_from_versioned_tx(tx: &VersionedTransaction) -> packet::Packet {
    let data = bincode::serialize(tx).expect("Failed to serialize transaction");
    let size = data.len() as u64;
    packet::Packet {
        data,
        meta: Some(packet::Meta {
            size,
            addr: String::new(),
            port: 0,
            flags: None,
            sender_stake: 0,
        }),
    }
}
