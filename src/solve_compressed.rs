use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs::File;
use std::time::Instant;

const TABLE_SIZE: usize = 16384;
const TABLE_MASK: usize = TABLE_SIZE - 1;

#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Entry {
    name_len: u16,
    min: i16,
    max: i16,
    _pad: u16,
    count: u32,
    sum: i64,
    name: [u8; 44],
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            name_len: 0,
            min: i16::MAX,
            max: i16::MIN,
            _pad: 0,
            count: 0,
            sum: 0,
            name: [0u8; 44],
        }
    }
}

#[inline(always)]
fn fast_hash(name: &[u8]) -> u64 {
    let len = name.len();
    if len >= 8 {
        let first = u64::from_le_bytes(name[..8].try_into().unwrap());
        let last = u64::from_le_bytes(name[len - 8..].try_into().unwrap());
        first.wrapping_mul(0x517cc1b727220a95) ^ last
    } else if len >= 4 {
        let first = u32::from_le_bytes(name[..4].try_into().unwrap()) as u64;
        let last = u32::from_le_bytes(name[len - 4..].try_into().unwrap()) as u64;
        (first | (last << 32)).wrapping_mul(0x517cc1b727220a95)
    } else {
        let b0 = name[0] as u64;
        let b1 = if len > 1 { name[1] as u64 } else { 0 };
        let b2 = if len > 2 { name[2] as u64 } else { 0 };
        (b0 | (b1 << 8) | (b2 << 16)).wrapping_mul(0x517cc1b727220a95)
    }
}

#[inline(always)]
fn parse_temp_fast(bytes: &[u8]) -> i16 {
    let len = bytes.len();
    if len == 3 {
        let d1 = (bytes[0] - b'0') as i16;
        let d2 = (bytes[2] - b'0') as i16;
        d1 * 10 + d2
    } else if len == 4 {
        if bytes[0] == b'-' {
            let d1 = (bytes[1] - b'0') as i16;
            let d2 = (bytes[3] - b'0') as i16;
            -(d1 * 10 + d2)
        } else {
            let d1 = (bytes[0] - b'0') as i16;
            let d2 = (bytes[1] - b'0') as i16;
            let d3 = (bytes[3] - b'0') as i16;
            d1 * 100 + d2 * 10 + d3
        }
    } else if len == 5 {
        let d1 = (bytes[1] - b'0') as i16;
        let d2 = (bytes[2] - b'0') as i16;
        let d3 = (bytes[4] - b'0') as i16;
        -(d1 * 100 + d2 * 10 + d3)
    } else {
        0
    }
}

#[inline(always)]
fn process_decompressed_chunk(chunk: &[u8], table: &mut [Entry]) {
    let mut cursor = 0;
    let len = chunk.len();

    while cursor < len {
        let name_start = cursor;
        while cursor < len && chunk[cursor] != b';' {
            cursor += 1;
        }
        if cursor >= len {
            break;
        }
        let name = &chunk[name_start..cursor];
        cursor += 1;

        let temp_start = cursor;
        while cursor < len && chunk[cursor] != b'\n' {
            cursor += 1;
        }
        let mut temp_end = cursor;
        if temp_end > temp_start && chunk[temp_end - 1] == b'\r' {
            temp_end -= 1;
        }
        let temp_bytes = &chunk[temp_start..temp_end];
        cursor += 1;

        let temp = parse_temp_fast(temp_bytes);
        let hash = fast_hash(name);

        let mut slot = (hash as usize) & TABLE_MASK;
        loop {
            let entry = unsafe { table.get_unchecked_mut(slot) };
            if entry.count == 0 {
                let nlen = name.len().min(44);
                entry.name_len = nlen as u16;
                entry.name[..nlen].copy_from_slice(&name[..nlen]);
                entry.min = temp;
                entry.max = temp;
                entry.sum = temp as i64;
                entry.count = 1;
                break;
            } else if entry.name_len as usize == name.len() && &entry.name[..name.len()] == name {
                if temp < entry.min {
                    entry.min = temp;
                }
                if temp > entry.max {
                    entry.max = temp;
                }
                entry.sum += temp as i64;
                entry.count += 1;
                break;
            }
            slot = (slot + 1) & TABLE_MASK;
        }
    }
}

#[derive(Clone)]
struct BlockIndexItem {
    offset: u64,
    comp_size: u32,
    uncomp_size: u32,
    bitset: [u64; 2],
}

struct FinalAgg {
    min: i16,
    max: i16,
    sum: i64,
    count: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let base_name = if args.len() > 1 {
        args[1].trim_end_matches(".txt").trim_end_matches(".zst_blocks").to_string()
    } else {
        "measurements_100m".to_string()
    };

    let mode = if args.len() > 2 {
        args[2].to_lowercase()
    } else {
        "all".to_string()
    };

    let target_station = if args.len() > 3 {
        args[3].clone()
    } else {
        "Hanoi".to_string()
    };

    let blocks_file = format!("{}.zst_blocks", base_name);
    let index_file = format!("{}.station_idx", base_name);

    println!("============================================================");
    println!("  1BRC BLOCK-ZSTD + STATION INDEX ENGINE");
    println!("  Block File:   {}", blocks_file);
    println!("  Index File:   {}", index_file);
    println!("  Mode:         {}", mode);
    if mode == "point" {
        println!("  Target Query: '{}'", target_station);
    }
    println!("============================================================");

    let start_total = Instant::now();

    // 1. Read Index File
    let start_idx = Instant::now();
    let idx_f = File::open(&index_file)?;
    let idx_mmap = unsafe { Mmap::map(&idx_f)? };

    if &idx_mmap[0..8] != b"1BRCIDX1" {
        return Err("Invalid Index magic header".into());
    }

    let num_stations = u32::from_le_bytes(idx_mmap[8..12].try_into()?) as usize;
    let num_blocks = u64::from_le_bytes(idx_mmap[12..20].try_into()?) as usize;

    let mut cursor = 20;
    let mut station_names = HashMap::new();
    let mut name_to_id = HashMap::new();

    for _ in 0..num_stations {
        let id = idx_mmap[cursor];
        cursor += 1;
        let nlen = u16::from_le_bytes(idx_mmap[cursor..cursor + 2].try_into()?) as usize;
        cursor += 2;
        let name = String::from_utf8_lossy(&idx_mmap[cursor..cursor + nlen]).to_string();
        cursor += nlen;
        station_names.insert(id, name.clone());
        name_to_id.insert(name, id);
    }

    // Read Block Metas
    let mut block_items = Vec::with_capacity(num_blocks);
    for _ in 0..num_blocks {
        let offset = u64::from_le_bytes(idx_mmap[cursor..cursor + 8].try_into()?);
        let comp_size = u32::from_le_bytes(idx_mmap[cursor + 8..cursor + 12].try_into()?);
        let uncomp_size = u32::from_le_bytes(idx_mmap[cursor + 12..cursor + 16].try_into()?);
        let b0 = u64::from_le_bytes(idx_mmap[cursor + 16..cursor + 24].try_into()?);
        let b1 = u64::from_le_bytes(idx_mmap[cursor + 24..cursor + 32].try_into()?);
        cursor += 32;

        block_items.push(BlockIndexItem {
            offset,
            comp_size,
            uncomp_size,
            bitset: [b0, b1],
        });
    }

    let idx_elapsed = start_idx.elapsed();
    println!("  -> Tải xong Index: {} trạm, {} khối dữ liệu trong {:.2}ms",
        num_stations, num_blocks, idx_elapsed.as_secs_f64() * 1000.0
    );

    // 2. Open Blocks File
    let blk_f = File::open(&blocks_file)?;
    let blk_len = blk_f.metadata()?.len();
    let blk_mmap = unsafe { Mmap::map(&blk_f)? };

    let raw_len = u64::from_le_bytes(blk_mmap[16..24].try_into()?);
    println!("  -> Kích thước nén trên SSD: {:.2} MB ({:.2} GB) [Gốc: {:.2} GB]",
        blk_len as f64 / 1_048_576.0, blk_len as f64 / 1_073_741_824.0,
        raw_len as f64 / 1_073_741_824.0
    );

    if mode == "point" {
        // MODE 2: POINT LOOKUP / SPECIFIC STATION QUERY
        let target_id = match name_to_id.get(&target_station) {
            Some(&id) => id,
            None => {
                println!("  [!] Không tìm thấy trạm '{}' trong từ điển trạm!", target_station);
                return Ok(());
            }
        };

        let word = (target_id / 64) as usize;
        let mask = 1u64 << (target_id % 64);

        let start_filter = Instant::now();
        // Filter blocks containing target station
        let matched_blocks: Vec<&BlockIndexItem> = block_items
            .iter()
            .filter(|b| (b.bitset[word] & mask) != 0)
            .collect();

        let filter_elapsed = start_filter.elapsed();
        let skipped_blocks = num_blocks - matched_blocks.len();
        let skip_pct = (skipped_blocks as f64 / num_blocks as f64) * 100.0;

        println!("  -> Khối bị loại bỏ bởi Index (Không cần đọc/giải nén): {}/{} khối ({:.2}%)",
            skipped_blocks, num_blocks, skip_pct
        );
        println!("  -> Số khối chứa '{}': {} khối", target_station, matched_blocks.len());

        let start_decomp = Instant::now();
        let num_threads = rayon::current_num_threads();

        // Process only matched blocks in parallel
        let target_bytes = target_station.as_bytes();
        let partial_aggs: Vec<Option<FinalAgg>> = matched_blocks
            .par_iter()
            .map(|b| {
                let start = (b.offset + 8) as usize;
                let end = start + b.comp_size as usize;
                let comp_data = &blk_mmap[start..end];

                let mut decomp_buf = vec![0u8; b.uncomp_size as usize];
                if zstd::bulk::decompress_to_buffer(comp_data, &mut decomp_buf).is_err() {
                    return None;
                }

                // Scan decompressed buffer for target station
                let mut cur = 0;
                let dlen = decomp_buf.len();
                let mut min = i16::MAX;
                let mut max = i16::MIN;
                let mut sum: i64 = 0;
                let mut count: u64 = 0;

                while cur < dlen {
                    let nstart = cur;
                    while cur < dlen && decomp_buf[cur] != b';' {
                        cur += 1;
                    }
                    if cur >= dlen {
                        break;
                    }
                    let name = &decomp_buf[nstart..cur];
                    cur += 1;

                    let tstart = cur;
                    while cur < dlen && decomp_buf[cur] != b'\n' {
                        cur += 1;
                    }
                    let mut tend = cur;
                    if tend > tstart && decomp_buf[tend - 1] == b'\r' {
                        tend -= 1;
                    }
                    let tbytes = &decomp_buf[tstart..tend];
                    cur += 1;

                    if name == target_bytes {
                        let t = parse_temp_fast(tbytes);
                        if t < min {
                            min = t;
                        }
                        if t > max {
                            max = t;
                        }
                        sum += t as i64;
                        count += 1;
                    }
                }

                if count > 0 {
                    Some(FinalAgg { min, max, sum, count })
                } else {
                    None
                }
            })
            .collect();

        // Combine partial aggregations
        let mut final_min = i16::MAX;
        let mut final_max = i16::MIN;
        let mut final_sum: i64 = 0;
        let mut final_count: u64 = 0;

        for agg_opt in partial_aggs {
            if let Some(agg) = agg_opt {
                if agg.min < final_min {
                    final_min = agg.min;
                }
                if agg.max > final_max {
                    final_max = agg.max;
                }
                final_sum += agg.sum;
                final_count += agg.count;
            }
        }

        let total_query_time = start_total.elapsed();

        println!("============================================================");
        println!("  KẾT QUẢ TRUY VẤN ĐIỂM (POINT QUERY): '{}'", target_station);
        println!("------------------------------------------------------------");
        let min_f = final_min as f64 / 10.0;
        let mean_f = if final_count > 0 { (final_sum as f64 / final_count as f64) / 10.0 } else { 0.0 };
        let max_f = final_max as f64 / 10.0;
        println!("  {}: min={:.1} / mean={:.1} / max={:.1} ({} bản ghi)", target_station, min_f, mean_f, max_f, final_count);
        println!("============================================================");
        println!("  HIỆU NĂNG TRUY VẤN ĐIỂM (POINT LOOKUP BENCHMARK):");
        println!("  Thời gian quét Index bitset: {:.2}ms", filter_elapsed.as_secs_f64() * 1000.0);
        println!("  Thời gian giải nén & tính:   {:.2}ms", start_decomp.elapsed().as_secs_f64() * 1000.0);
        println!("  TỔNG THỜI GIAN TRUY VẤN:     {:.3}s ({:.2}ms)", total_query_time.as_secs_f64(), total_query_time.as_secs_f64() * 1000.0);
        println!("  So với 1BRC chuẩn (8.23s):   Nhanh hơn gấp {:.1} lần!",
            8.237 / total_query_time.as_secs_f64()
        );
        println!("============================================================");

    } else {
        // MODE 1: FULL 1BRC AGGREGATION FROM BLOCK-ZSTD
        println!("  -> Chế độ: GIẢI NÉN VÀ TÍNH TOÁN TOÀN BỘ BẰNG 20 LUỒNG SONG SONG");

        let start_decomp_all = Instant::now();
        let num_threads = rayon::current_num_threads();

        // Divide blocks into thread partitions
        let blocks_per_thread = (num_blocks + num_threads - 1) / num_threads;

        let tables: Vec<Vec<Entry>> = (0..num_threads)
            .into_par_iter()
            .map(|tid| {
                let start_idx = tid * blocks_per_thread;
                let end_idx = ((tid + 1) * blocks_per_thread).min(num_blocks);

                let mut table = vec![Entry::default(); TABLE_SIZE];
                let mut decomp_buf = vec![0u8; 128 * 1024]; // reuse 128KB buffer per thread

                for i in start_idx..end_idx {
                    let b = &block_items[i];
                    let comp_start = (b.offset + 8) as usize;
                    let comp_end = comp_start + b.comp_size as usize;
                    let comp_data = &blk_mmap[comp_start..comp_end];

                    let uncomp_size = b.uncomp_size as usize;
                    if decomp_buf.len() < uncomp_size {
                        decomp_buf.resize(uncomp_size, 0);
                    }

                    if zstd::bulk::decompress_to_buffer(comp_data, &mut decomp_buf[..uncomp_size]).is_ok() {
                        process_decompressed_chunk(&decomp_buf[..uncomp_size], &mut table);
                    }
                }

                table
            })
            .collect();

        let calc_elapsed = start_decomp_all.elapsed();

        // Merge tables
        let start_merge = Instant::now();
        let mut combined: BTreeMap<String, FinalAgg> = BTreeMap::new();

        for table in tables {
            for entry in table {
                if entry.count > 0 {
                    let name = String::from_utf8_lossy(&entry.name[..entry.name_len as usize]).to_string();
                    let agg = combined.entry(name).or_insert(FinalAgg {
                        min: i16::MAX,
                        max: i16::MIN,
                        sum: 0,
                        count: 0,
                    });
                    if entry.min < agg.min {
                        agg.min = entry.min;
                    }
                    if entry.max > agg.max {
                        agg.max = entry.max;
                    }
                    agg.sum += entry.sum;
                    agg.count += entry.count as u64;
                }
            }
        }

        let merge_elapsed = start_merge.elapsed();
        let total_elapsed = start_total.elapsed();

        println!("============================================================");
        println!("  KẾT QUẢ TÍNH TOÁN 1BRC TỪ BLOCK-ZSTD (Mẫu 10 trạm):");
        println!("------------------------------------------------------------");

        let mut printed = 0;
        for (name, agg) in combined.iter() {
            let min_f = agg.min as f64 / 10.0;
            let mean_f = (agg.sum as f64 / agg.count as f64) / 10.0;
            let max_f = agg.max as f64 / 10.0;
            println!("  {}: min={:.1} / mean={:.1} / max={:.1} ({} bản ghi)", name, min_f, mean_f, max_f, agg.count);
            printed += 1;
            if printed >= 10 {
                break;
            }
        }
        if combined.len() > 10 {
            println!("  ... và {} trạm thời tiết khác.", combined.len() - 10);
        }

        let total_rows: u64 = combined.values().map(|a| a.count).sum();

        println!("============================================================");
        println!("  HIỆU NĂNG TÍNH TOÁN TOÀN BỘ TỪ BLOCK-ZSTD:");
        println!("  Tổng số dòng xử lý:   {} dòng", total_rows);
        println!("  Dung lượng đọc từ SSD:{:.2} MB (giảm {:.1}% so với raw {:.2} MB)",
            blk_len as f64 / 1_048_576.0,
            (1.0 - blk_len as f64 / raw_len as f64) * 100.0,
            raw_len as f64 / 1_048_576.0
        );
        println!("  Thời gian giải nén + tính: {:.3}s ({:.2}ms)", calc_elapsed.as_secs_f64(), calc_elapsed.as_secs_f64() * 1000.0);
        println!("  Thời gian gộp kết quả:     {:.3}s ({:.2}ms)", merge_elapsed.as_secs_f64(), merge_elapsed.as_secs_f64() * 1000.0);
        println!("  TỔNG THỜI GIAN (A-Z):      {:.3}s ({:.2}ms)", total_elapsed.as_secs_f64(), total_elapsed.as_secs_f64() * 1000.0);
        println!("  Tốc độ xử lý (Throughput): {:.2} Triệu dòng / giây ({:.2} GB raw/s)",
            (total_rows as f64 / total_elapsed.as_secs_f64()) / 1_000_000.0,
            (raw_len as f64 / 1_073_741_824.0) / total_elapsed.as_secs_f64()
        );
        println!("============================================================");
    }

    Ok(())
}
