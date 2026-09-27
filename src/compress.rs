use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Instant;

const BLOCK_TARGET_SIZE: usize = 64 * 1024; // 64 KB uncompressed
const BATCH_SIZE: usize = 4096; // 4096 blocks per streaming batch (~256 MB raw, ~75 MB compressed)

struct BlockMeta {
    offset: u64,
    comp_size: u32,
    uncomp_size: u32,
    station_bitset: [u64; 2],
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let input_file = if args.len() > 1 {
        args[1].clone()
    } else {
        "measurements_100m.txt".to_string()
    };

    let base_name = input_file.trim_end_matches(".txt");
    let output_blocks = format!("{}.zst_blocks", base_name);
    let output_index = format!("{}.station_idx", base_name);

    println!("============================================================");
    println!("  1BRC BLOCK-ZSTD + STATION BITSET INDEX GENERATOR (STREAMING)");
    println!("  Input:         {}", input_file);
    println!("  Block Output:  {}", output_blocks);
    println!("  Index Output:  {}", output_index);
    println!("============================================================");

    let start_total = Instant::now();

    let file = File::open(&input_file)?;
    let file_len = file.metadata()?.len();
    let mmap = unsafe { Mmap::map(&file)? };

    println!("  -> Tệp nguồn: {:.2} MB ({:.2} GB)", file_len as f64 / 1_048_576.0, file_len as f64 / 1_073_741_824.0);

    // 1. First pass: Collect all unique station names to assign dense Station IDs (0..127)
    let start_scan = Instant::now();
    let mut station_map: HashMap<String, u8> = HashMap::new();
    let mut station_list: Vec<String> = Vec::new();

    let probe_limit = (file_len as usize).min(10 * 1024 * 1024);
    let mut cursor = 0;
    while cursor < probe_limit {
        let start = cursor;
        while cursor < probe_limit && mmap[cursor] != b';' {
            cursor += 1;
        }
        if cursor >= probe_limit {
            break;
        }
        let name = String::from_utf8_lossy(&mmap[start..cursor]).to_string();
        if !station_map.contains_key(&name) {
            let id = station_list.len() as u8;
            station_map.insert(name.clone(), id);
            station_list.push(name);
        }
        while cursor < probe_limit && mmap[cursor] != b'\n' {
            cursor += 1;
        }
        cursor += 1;
    }

    println!("  -> Đã nhận diện: {} trạm thời tiết độc nhất trong {:.2}ms", station_list.len(), start_scan.elapsed().as_secs_f64() * 1000.0);

    // 2. Partition file into 64KB chunks aligned to newlines
    let start_partition = Instant::now();
    let mut block_slices = Vec::new();
    let mut offset = 0;
    while offset < file_len as usize {
        let mut end = (offset + BLOCK_TARGET_SIZE).min(file_len as usize);
        if end < file_len as usize {
            while end < file_len as usize && mmap[end] != b'\n' {
                end += 1;
            }
            if end < file_len as usize {
                end += 1;
            }
        }
        block_slices.push((offset, end));
        offset = end;
    }

    let num_blocks = block_slices.len();
    println!("  -> Đã chia thành: {} khối dữ liệu 64KB trong {:.2}ms", num_blocks, start_partition.elapsed().as_secs_f64() * 1000.0);

    // 3. Initialize Writers
    let out_file = File::create(&output_blocks)?;
    let mut writer = BufWriter::with_capacity(32 * 1024 * 1024, out_file);

    // Header: Magic "1BRCZST1", num_blocks (u64), raw_len (u64)
    writer.write_all(b"1BRCZST1")?;
    writer.write_all(&(num_blocks as u64).to_le_bytes())?;
    writer.write_all(&(file_len as u64).to_le_bytes())?;

    let mut block_metas: Vec<BlockMeta> = Vec::with_capacity(num_blocks);
    let mut current_file_offset = 8 + 8 + 8; // magic + num_blocks + raw_len

    // 4. Stream compress in batches of BATCH_SIZE to preserve RAM!
    let start_comp = Instant::now();
    let mut processed_count = 0;

    for chunk_batch in block_slices.chunks(BATCH_SIZE) {
        let batch_results: Vec<(Vec<u8>, u32, [u64; 2])> = chunk_batch
            .par_iter()
            .map(|&(start, end)| {
                let chunk = &mmap[start..end];
                let uncomp_size = chunk.len() as u32;

                let mut bitset: [u64; 2] = [0, 0];
                let mut cur = 0;
                while cur < chunk.len() {
                    let name_start = cur;
                    while cur < chunk.len() && chunk[cur] != b';' {
                        cur += 1;
                    }
                    if cur >= chunk.len() {
                        break;
                    }
                    let name = &chunk[name_start..cur];
                    if let Ok(name_str) = std::str::from_utf8(name) {
                        if let Some(&id) = station_map.get(name_str) {
                            let word = (id / 64) as usize;
                            let bit = id % 64;
                            bitset[word] |= 1 << bit;
                        }
                    }
                    while cur < chunk.len() && chunk[cur] != b'\n' {
                        cur += 1;
                    }
                    cur += 1;
                }

                let compressed = zstd::bulk::compress(chunk, 1).expect("zstd compression");
                (compressed, uncomp_size, bitset)
            })
            .collect();

        // Write batch to disk immediately
        for (comp_data, uncomp_size, bitset) in batch_results {
            let comp_size = comp_data.len() as u32;
            writer.write_all(&comp_size.to_le_bytes())?;
            writer.write_all(&uncomp_size.to_le_bytes())?;
            writer.write_all(&comp_data)?;

            block_metas.push(BlockMeta {
                offset: current_file_offset,
                comp_size,
                uncomp_size,
                station_bitset: bitset,
            });

            current_file_offset += 4 + 4 + comp_size as u64;
        }

        processed_count += chunk_batch.len();
        if processed_count % (BATCH_SIZE * 4) == 0 || processed_count == num_blocks {
            let pct = (processed_count as f64 / num_blocks as f64) * 100.0;
            println!("  -> Tiến độ nén: {} / {} khối ({:.1}%) - {:.1}s",
                processed_count, num_blocks, pct, start_comp.elapsed().as_secs_f64()
            );
        }
    }
    writer.flush()?;

    let comp_elapsed = start_comp.elapsed();
    println!("  -> Nén xong toàn bộ trong {:.2}s ({:.2} MB/s)",
        comp_elapsed.as_secs_f64(),
        (file_len as f64 / 1_048_576.0) / comp_elapsed.as_secs_f64()
    );

    // 5. Write .station_idx index file
    let idx_file = File::create(&output_index)?;
    let mut idx_writer = BufWriter::with_capacity(8 * 1024 * 1024, idx_file);

    idx_writer.write_all(b"1BRCIDX1")?;
    idx_writer.write_all(&(station_list.len() as u32).to_le_bytes())?;
    idx_writer.write_all(&(num_blocks as u64).to_le_bytes())?;

    for (id, name) in station_list.iter().enumerate() {
        idx_writer.write_all(&(id as u8).to_le_bytes())?;
        let name_bytes = name.as_bytes();
        idx_writer.write_all(&(name_bytes.len() as u16).to_le_bytes())?;
        idx_writer.write_all(name_bytes)?;
    }

    for meta in &block_metas {
        idx_writer.write_all(&meta.offset.to_le_bytes())?;
        idx_writer.write_all(&meta.comp_size.to_le_bytes())?;
        idx_writer.write_all(&meta.uncomp_size.to_le_bytes())?;
        idx_writer.write_all(&meta.station_bitset[0].to_le_bytes())?;
        idx_writer.write_all(&meta.station_bitset[1].to_le_bytes())?;
    }
    idx_writer.flush()?;

    let total_elapsed = start_total.elapsed();
    let comp_file_size = std::fs::metadata(&output_blocks)?.len();
    let idx_file_size = std::fs::metadata(&output_index)?.len();

    println!("============================================================");
    println!("  HOÀN THÀNH TẠO BLOCK-ZSTD & STATION INDEX!");
    println!("  Dung lượng gốc:     {:.2} MB ({:.2} GB)", file_len as f64 / 1_048_576.0, file_len as f64 / 1_073_741_824.0);
    println!("  Dung lượng nén:     {:.2} MB ({:.2} GB) [Tỷ lệ: {:.1}%]",
        comp_file_size as f64 / 1_048_576.0, comp_file_size as f64 / 1_073_741_824.0,
        (comp_file_size as f64 / file_len as f64) * 100.0
    );
    println!("  Dung lượng Index:   {:.2} MB ({:.2} KB)",
        idx_file_size as f64 / 1_048_576.0, idx_file_size as f64 / 1024.0
    );
    println!("  Tiết kiệm ổ cứng:   {:.2} GB ({:.1}%)",
        (file_len - comp_file_size - idx_file_size) as f64 / 1_073_741_824.0,
        (1.0 - (comp_file_size + idx_file_size) as f64 / file_len as f64) * 100.0
    );
    println!("  Tổng thời gian:     {:.2}s", total_elapsed.as_secs_f64());
    println!("============================================================");

    Ok(())
}
