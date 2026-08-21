//! Monotonic UUIDv7 generation ⇐ pi `src/utils/uuid.ts`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy)]
struct UuidState {
    last_timestamp: i64,
    sequence: u32,
}

static UUID_STATE: Mutex<UuidState> = Mutex::new(UuidState {
    last_timestamp: i64::MIN,
    sequence: 0,
});
static FALLBACK_RANDOM: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

fn fallback_random(random: &mut [u8]) {
    let mut state = FALLBACK_RANDOM.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
        ^ u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        )
        .unwrap_or(u64::MAX);
    for byte in random {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
}

fn uuid_v7_with(timestamp: i64, random: [u8; 16], state: &mut UuidState) -> String {
    if timestamp > state.last_timestamp {
        state.sequence = u32::from_be_bytes([random[6], random[7], random[8], random[9]]);
        state.last_timestamp = timestamp;
    } else {
        state.sequence = state.sequence.wrapping_add(1);
        if state.sequence == 0 {
            state.last_timestamp += 1;
        }
    }

    let timestamp = state.last_timestamp as u64;
    let sequence = state.sequence;
    let mut bytes = [0_u8; 16];
    bytes[0] = (timestamp >> 40) as u8;
    bytes[1] = (timestamp >> 32) as u8;
    bytes[2] = (timestamp >> 24) as u8;
    bytes[3] = (timestamp >> 16) as u8;
    bytes[4] = (timestamp >> 8) as u8;
    bytes[5] = timestamp as u8;
    bytes[6] = 0x70 | ((sequence >> 28) as u8 & 0x0f);
    bytes[7] = (sequence >> 20) as u8;
    bytes[8] = 0x80 | ((sequence >> 14) as u8 & 0x3f);
    bytes[9] = (sequence >> 6) as u8;
    bytes[10] = (((sequence & 0x3f) as u8) << 2) | (random[10] & 0x03);
    bytes[11..].copy_from_slice(&random[11..]);
    uuid::Uuid::from_bytes(bytes).to_string()
}

pub fn uuid_v7() -> String {
    let mut random = [0_u8; 16];
    if getrandom::fill(&mut random).is_err() {
        fallback_random(&mut random);
    }
    let timestamp = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    uuid_v7_with(
        timestamp,
        random,
        &mut UUID_STATE.lock().expect("uuid state mutex poisoned"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_exact_sequence_layout_and_monotonicity() {
        let mut state = UuidState {
            last_timestamp: i64::MIN,
            sequence: 0,
        };
        let random = [
            0, 1, 2, 3, 4, 5, 0x12, 0x34, 0x56, 0x78, 0xab, 11, 12, 13, 14, 15,
        ];
        let first = uuid_v7_with(0x0102_0304_0506, random, &mut state);
        assert_eq!(first, "01020304-0506-7123-9159-e30b0c0d0e0f");
        let second = uuid_v7_with(0x0102_0304_0506, random, &mut state);
        assert!(second > first);
        assert_eq!(
            uuid::Uuid::parse_str(&second)
                .expect("uuid")
                .get_version_num(),
            7
        );
    }
}
