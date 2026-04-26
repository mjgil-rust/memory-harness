use std::thread;
use std::time::Duration;

fn main() {
    let iters = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8);
    let chunk_kib = std::env::args()
        .nth(2)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(512);
    let sleep_ms = std::env::args()
        .nth(3)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(20);

    let mut chunks = Vec::new();
    for idx in 0..iters {
        let mut chunk = vec![0_u8; chunk_kib * 1024];
        chunk.fill((idx % 251) as u8);
        chunks.push(chunk);
        thread::sleep(Duration::from_millis(sleep_ms));
    }

    for _ in 0..(iters / 2) {
        let _ = chunks.pop();
        thread::sleep(Duration::from_millis(sleep_ms));
    }

    let checksum: u64 = chunks
        .iter()
        .flat_map(|chunk| chunk.iter().take(32))
        .map(|byte| u64::from(*byte))
        .sum();
    println!("checksum={checksum} remaining_chunks={}", chunks.len());
}
