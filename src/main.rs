use memmap2::Mmap;
use std::arch::x86_64::*;
use std::collections::BTreeMap;
use std::fs::File;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

const TABLE_SIZE: usize = 16384;
const TABLE_MASK: usize = TABLE_SIZE - 1;

#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Entry {
    hash: u64,
    name_ptr: *const u8,
    name_len: u32,
    min: i16,
    max: i16,
    count: u32,
    sum: i64,
    name_prefix: [u8; 24],
}

const _: () = assert!(std::mem::size_of::<Entry>() == 64);

impl Default for Entry {
    fn default() -> Self {
        Self {
            hash: 0,
            name_ptr: std::ptr::null(),
            name_len: 0,
            min: i16::MAX,
            max: i16::MIN,
            count: 0,
            sum: 0,
            name_prefix: [0u8; 24],
        }
    }
}

unsafe impl Send for Entry {}
unsafe impl Sync for Entry {}

#[inline(always)]
fn rounded_mean_tenths(sum: i64, count: u64) -> i64 {
    let s = sum as i128;
    let n = count as i128;
    ((2 * s + n).div_euclid(2 * n)) as i64
}

#[inline(always)]
fn format_tenths(val: i64) -> String {
    if val < 0 {
        let abs = -val;
        format!("-{}.{}", abs / 10, abs % 10)
    } else {
        format!("{}.{}", val / 10, val % 10)
    }
}

#[inline(always)]
unsafe fn fast_hash_raw(ptr: *const u8, len: usize) -> u64 {
    if len < 8 {
        let mut buf = [0u8; 8];
        std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), len);
        let word = u64::from_le_bytes(buf);
        return word.wrapping_mul(0x517cc1b727220a95);
    }

    if len <= 16 {
        let first = std::ptr::read_unaligned(ptr as *const u64);
        let last = std::ptr::read_unaligned(ptr.add(len - 8) as *const u64);
        return (first ^ last.rotate_left(17))
            .wrapping_add((len as u64).wrapping_mul(0x9e3779b97f4a7c15))
            .wrapping_mul(0x517cc1b727220a95);
    }

    if len <= 32 {
        let w0 = std::ptr::read_unaligned(ptr as *const u64);
        let w1 = std::ptr::read_unaligned(ptr.add(8) as *const u64);
        let w2 = std::ptr::read_unaligned(ptr.add(len - 16) as *const u64);
        let w3 = std::ptr::read_unaligned(ptr.add(len - 8) as *const u64);
        let h = w0 ^ w1.rotate_left(13) ^ w2.rotate_left(27) ^ w3.rotate_left(41) ^ (len as u64).wrapping_mul(0x9e3779b97f4a7c15);
        return h.wrapping_mul(0x517cc1b727220a95);
    }

    // For len > 32 (up to 100 bytes): full-coverage linear fold reading every 8-byte chunk
    // GUARANTEES zero skipped bytes across any string length!
    let mut h = (len as u64).wrapping_mul(0x9e3779b97f4a7c15);
    let mut offset = 0;
    while offset + 8 <= len {
        let word = std::ptr::read_unaligned(ptr.add(offset) as *const u64);
        h = (h ^ word.rotate_left(17)).wrapping_mul(0x517cc1b727220a95);
        offset += 8;
    }
    if offset < len {
        let last = std::ptr::read_unaligned(ptr.add(len - 8) as *const u64);
        h = (h ^ last.rotate_left(31)).wrapping_mul(0x517cc1b727220a95);
    }
    h
}

#[inline(always)]
unsafe fn update_table(table: &mut [Entry], name_ptr: *const u8, name_len: usize, temp: i16) {
    let hash = fast_hash_raw(name_ptr, name_len);
    // Use high 14 bits of 64-bit product (avoids lower-bit prefix collision vulnerability)
    let mut slot = ((hash >> 50) as usize) & TABLE_MASK;

    loop {
        let entry = table.get_unchecked_mut(slot);
        if entry.count == 0 {
            entry.hash = hash;
            entry.name_ptr = name_ptr;
            entry.name_len = name_len as u32;
            let copy_len = name_len.min(24);
            std::ptr::copy_nonoverlapping(name_ptr, entry.name_prefix.as_mut_ptr(), copy_len);
            entry.min = temp;
            entry.max = temp;
            entry.sum = temp as i64;
            entry.count = 1;
            return;
        }

        if entry.hash == hash && entry.name_len as usize == name_len {
            let match_found = if name_len <= 24 {
                let s1 = std::slice::from_raw_parts(name_ptr, name_len);
                let s2 = &entry.name_prefix[..name_len];
                s1 == s2
            } else {
                let s1 = std::slice::from_raw_parts(name_ptr, name_len);
                let s2 = std::slice::from_raw_parts(entry.name_ptr, name_len);
                s1 == s2
            };

            if match_found {
                entry.min = entry.min.min(temp);
                entry.max = entry.max.max(temp);
                entry.sum += temp as i64;
                entry.count += 1;
                return;
            }
        }

        slot = (slot + 1) & TABLE_MASK;
    }
}

#[inline(always)]
unsafe fn find_semi_avx2(mut ptr: *const u8) -> *const u8 {
    let semi_vec = _mm256_set1_epi8(b';' as i8);
    loop {
        let v = _mm256_loadu_si256(ptr as *const __m256i);
        let cmp = _mm256_cmpeq_epi8(v, semi_vec);
        let mask = _mm256_movemask_epi8(cmp) as u32;
        if mask != 0 {
            let offset = mask.trailing_zeros() as usize;
            return ptr.add(offset);
        }
        ptr = ptr.add(32);
    }
}

#[inline(always)]
unsafe fn parse_temp_and_next(ptr: *const u8) -> (i16, *const u8) {
    let b0 = *ptr;
    let (neg, p) = if b0 == b'-' {
        (true, ptr.add(1))
    } else {
        (false, ptr)
    };

    let c0 = *p;
    let c1 = *p.add(1);

    let (val, next_p) = if c1 == b'.' {
        let c2 = *p.add(2);
        let v = (c0 - b'0') as i16 * 10 + (c2 - b'0') as i16;
        let advance = 4 + (*p.add(3) == b'\r') as usize;
        (v, p.add(advance))
    } else {
        let c3 = *p.add(3);
        let v = (c0 - b'0') as i16 * 100 + (c1 - b'0') as i16 * 10 + (c3 - b'0') as i16;
        let advance = 5 + (*p.add(4) == b'\r') as usize;
        (v, p.add(advance))
    };

    (if neg { -val } else { val }, next_p)
}

#[inline(always)]
unsafe fn process_single_stream(table: &mut [Entry], mut ptr: *const u8, chunk_end: *const u8) {
    let chunk_len = (chunk_end as usize).saturating_sub(ptr as usize);
    let safe_end = if chunk_len >= 128 {
        chunk_end.sub(128)
    } else {
        ptr
    };

    while ptr < safe_end {
        let name_start = ptr;
        let semi_ptr = find_semi_avx2(name_start);
        let name_len = (semi_ptr as usize) - (name_start as usize);

        let temp_ptr = semi_ptr.add(1);
        let (temp, next_ptr) = parse_temp_and_next(temp_ptr);

        update_table(table, name_start, name_len, temp);
        ptr = next_ptr;
    }

    // Scalar fallback for remaining few bytes (< 128 bytes)
    while ptr < chunk_end {
        let name_start = ptr;
        let mut semi_ptr = ptr;
        while semi_ptr < chunk_end && *semi_ptr != b';' {
            semi_ptr = semi_ptr.add(1);
        }
        if semi_ptr >= chunk_end {
            break;
        }

        let name_len = (semi_ptr as usize) - (name_start as usize);
        let mut p = semi_ptr.add(1);

        let mut neg = false;
        if p < chunk_end && *p == b'-' {
            neg = true;
            p = p.add(1);
        }

        let mut val: i16 = 0;
        while p < chunk_end && *p != b'\n' && *p != b'\r' {
            if *p >= b'0' && *p <= b'9' {
                val = val * 10 + (*p - b'0') as i16;
            }
            p = p.add(1);
        }

        let temp = if neg { -val } else { val };
        update_table(table, name_start, name_len, temp);

        while p < chunk_end && (*p == b'\n' || *p == b'\r') {
            p = p.add(1);
        }
        ptr = p;
    }
}

unsafe fn process_chunk_fast(table: &mut [Entry], chunk_start: *const u8, chunk_end: *const u8) {
    let chunk_len = (chunk_end as usize).saturating_sub(chunk_start as usize);

    // If chunk is small (< 8KB), fall back to single stream
    if chunk_len < 8192 {
        process_single_stream(table, chunk_start, chunk_end);
        return;
    }

    // Split chunk into 2 independent sub-streams for 2-way Instruction-Level Parallelism (ILP)
    let mid_approx = chunk_start.add(chunk_len / 2);
    let mut mid = mid_approx;
    while mid < chunk_end && *mid != b'\n' {
        mid = mid.add(1);
    }
    if mid < chunk_end {
        mid = mid.add(1); // align to line start
    }

    if mid >= chunk_end || mid <= chunk_start {
        process_single_stream(table, chunk_start, chunk_end);
        return;
    }

    let mut ptr0 = chunk_start;
    let mut ptr1 = mid;

    let len0 = (mid as usize).saturating_sub(ptr0 as usize);
    let safe_end0 = if len0 >= 128 { mid.sub(128) } else { ptr0 };

    let len1 = (chunk_end as usize).saturating_sub(ptr1 as usize);
    let safe_end1 = if len1 >= 128 { chunk_end.sub(128) } else { ptr1 };

    // 2-Way Interleaved SIMD & Hash Execution Loop
    // Exploits dual load ports (Ports 2 & 3) and independent ALU / AVX execution units
    while ptr0 < safe_end0 && ptr1 < safe_end1 {
        // Stream 0 - Parse line
        let name0 = ptr0;
        let semi0 = find_semi_avx2(name0);
        let len0 = (semi0 as usize) - (name0 as usize);
        let (temp0, next0) = parse_temp_and_next(semi0.add(1));

        // Stream 1 - Parse line (independent execution pipeline)
        let name1 = ptr1;
        let semi1 = find_semi_avx2(name1);
        let len1 = (semi1 as usize) - (name1 as usize);
        let (temp1, next1) = parse_temp_and_next(semi1.add(1));

        // Stream 0 & 1 - Update hash table
        update_table(table, name0, len0, temp0);
        update_table(table, name1, len1, temp1);

        ptr0 = next0;
        ptr1 = next1;
    }

    // Drain remaining portions of both streams
    process_single_stream(table, ptr0, mid);
    process_single_stream(table, ptr1, chunk_end);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let is_canonical = args.iter().any(|a| a == "--canonical") || std::env::var("CANONICAL").is_ok();
    let filename = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "measurements_100m.txt".to_string());

    let start_all = Instant::now();

    if !is_canonical {
        println!("============================================================");
        println!("  1BRC ULTRA-APEX SOLVER (AVX2 / SWAR / WORK-STEALING)");
        println!("  Target File: {}", filename);
        println!("============================================================");
    }

    let file = File::open(&filename)?;
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        if is_canonical {
            println!("{{}}");
        } else {
            println!("Tệp rỗng (0 bytes).");
        }
        return Ok(());
    }

    let mmap = unsafe { Mmap::map(&file)? };

    let num_threads = std::env::var("THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| thread::available_parallelism().map_or(1, usize::from));

    if !is_canonical {
        println!("  -> Kích thước tệp: {:.2} MB ({:.2} GB)", file_len as f64 / 1_048_576.0, file_len as f64 / 1_073_741_824.0);
        println!("  -> Số luồng CPU:   {} luồng song song (Work-Stealing)", num_threads);
    }

    let start_calc = Instant::now();

    let num_chunks = (num_threads * 4).min((file_len as usize).max(1));
    let approx_chunk_size = (file_len as usize) / num_chunks;
    let mut chunk_ranges: Vec<(usize, usize)> = Vec::with_capacity(num_chunks);

    let mut current_offset = 0usize;
    for i in 0..num_chunks {
        let start = current_offset;
        let mut end = if i == num_chunks - 1 {
            file_len as usize
        } else {
            ((i + 1) * approx_chunk_size).min(file_len as usize)
        };

        if end < file_len as usize {
            while end < file_len as usize && mmap[end] != b'\n' {
                end += 1;
            }
            if end < file_len as usize {
                end += 1; // skip '\n'
            }
        }

        if start < end {
            chunk_ranges.push((start, end));
        }
        current_offset = end;
        if current_offset >= file_len as usize {
            break;
        }
    }

    let chunk_idx = AtomicUsize::new(0);
    let total_chunks = chunk_ranges.len();
    let mmap_base = mmap.as_ptr() as usize;

    let tables: Vec<Vec<Entry>> = thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);

        for _ in 0..num_threads {
            let chunk_ranges_ref = &chunk_ranges;
            let chunk_idx_ref = &chunk_idx;

            handles.push(s.spawn(move || {
                let mut local_table = vec![Entry::default(); TABLE_SIZE];
                let mmap_ptr = mmap_base as *const u8;

                loop {
                    let idx = chunk_idx_ref.fetch_add(1, Ordering::Relaxed);
                    if idx >= total_chunks {
                        break;
                    }
                    let (start, end) = chunk_ranges_ref[idx];
                    unsafe {
                        process_chunk_fast(&mut local_table, mmap_ptr.add(start), mmap_ptr.add(end));
                    }
                }

                local_table
            }));
        }

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let calc_elapsed = start_calc.elapsed();

    // Merge thread-local tables
    let start_merge = Instant::now();
    let mut combined: BTreeMap<String, (i16, i16, i64, u64)> = BTreeMap::new();

    for table in tables {
        for entry in table {
            if entry.count > 0 {
                let name_slice = unsafe { std::slice::from_raw_parts(entry.name_ptr, entry.name_len as usize) };
                let name = String::from_utf8_lossy(name_slice).to_string();

                let agg = combined.entry(name).or_insert((i16::MAX, i16::MIN, 0, 0));
                if entry.min < agg.0 {
                    agg.0 = entry.min;
                }
                if entry.max > agg.1 {
                    agg.1 = entry.max;
                }
                agg.2 += entry.sum;
                agg.3 += entry.count as u64;
            }
        }
    }

    let merge_elapsed = start_merge.elapsed();
    let total_elapsed = start_all.elapsed();

    if is_canonical {
        let mut parts = Vec::with_capacity(combined.len());
        for (name, (min, max, sum, count)) in combined.iter() {
            let min_s = format_tenths(*min as i64);
            let mean_s = format_tenths(rounded_mean_tenths(*sum, *count));
            let max_s = format_tenths(*max as i64);
            parts.push(format!("{}={}/{}/{}", name, min_s, mean_s, max_s));
        }
        println!("{{{}}}", parts.join(", "));
        return Ok(());
    }

    println!("============================================================");
    println!("  KẾT QUẢ TÍNH TOÁN (10 trạm đầu tiên - chuẩn 1BRC Math.round):");
    println!("------------------------------------------------------------");

    let mut printed = 0;
    for (name, (min, max, sum, count)) in combined.iter() {
        let min_s = format_tenths(*min as i64);
        let mean_s = format_tenths(rounded_mean_tenths(*sum, *count));
        let max_s = format_tenths(*max as i64);
        println!("  {}: min={} / mean={} / max={} ({} bản ghi)", name, min_s, mean_s, max_s, count);
        printed += 1;
        if printed >= 10 {
            break;
        }
    }
    if combined.len() > 10 {
        println!("  ... và {} trạm thời tiết khác.", combined.len() - 10);
    }

    let total_rows: u64 = combined.values().map(|a| a.3).sum();

    println!("============================================================");
    println!("  HIỆU NĂNG ĐO ĐƯỢC (BENCHMARK):");
    println!("  Tổng số dòng xử lý:   {} dòng", total_rows);
    println!("  Thời gian tính toán:  {:.3}s ({:.2}ms)", calc_elapsed.as_secs_f64(), calc_elapsed.as_secs_f64() * 1000.0);
    println!("  Thời gian gộp & sắp:  {:.3}s ({:.2}ms)", merge_elapsed.as_secs_f64(), merge_elapsed.as_secs_f64() * 1000.0);
    println!("  TỔNG THỜI GIAN (A-Z): {:.3}s ({:.2}ms)", total_elapsed.as_secs_f64(), total_elapsed.as_secs_f64() * 1000.0);
    println!("  Tốc độ tính toán (Compute Throughput): {:.2} Triệu dòng / giây ({:.2} GiB/s)",
        (total_rows as f64 / calc_elapsed.as_secs_f64().max(0.0001)) / 1_000_000.0,
        (file_len as f64 / 1_073_741_824.0) / calc_elapsed.as_secs_f64().max(0.0001)
    );
    println!("  Tốc độ toàn trình (Total Throughput):   {:.2} Triệu dòng / giây ({:.2} GiB/s)",
        (total_rows as f64 / total_elapsed.as_secs_f64().max(0.0001)) / 1_000_000.0,
        (file_len as f64 / 1_073_741_824.0) / total_elapsed.as_secs_f64().max(0.0001)
    );
    println!("============================================================");

    Ok(())
}
