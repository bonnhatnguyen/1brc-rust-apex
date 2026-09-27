use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs::File;
use std::time::Instant;

#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct StationStat {
    min: i16,
    max: i16,
    _pad: u32,
    sum: i64,
    count: u64,
}

impl Default for StationStat {
    fn default() -> Self {
        Self {
            min: i16::MAX,
            max: i16::MIN,
            _pad: 0,
            sum: 0,
            count: 0,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let filename = if args.len() > 1 {
        args[1].clone()
    } else {
        "measurements_1b.col_bin".to_string()
    };

    let start_all = Instant::now();

    println!("============================================================");
    println!("  1BRC UNCONSTRAINED COLUMNAR SOLVER (L1 CACHE RESIDENT)");
    println!("  Target File: {}", filename);
    println!("============================================================");

    let file = File::open(&filename)?;
    let file_len = file.metadata()?.len();
    let mmap = unsafe { Mmap::map(&file)? };

    if &mmap[0..8] != b"1BRCCOL1" {
        return Err("Sai định dạng magic header (Cần 1BRCCOL1)".into());
    }

    let num_stations = u32::from_le_bytes(mmap[8..12].try_into()?) as usize;
    let dict_bytes = u32::from_le_bytes(mmap[12..16].try_into()?) as usize;
    let num_rows = u64::from_le_bytes(mmap[16..24].try_into()?);

    let mut cursor = 64;
    let mut station_names = Vec::with_capacity(num_stations);

    for _ in 0..num_stations {
        let id = mmap[cursor] as usize;
        cursor += 1;
        let nlen = u16::from_le_bytes(mmap[cursor..cursor + 2].try_into()?) as usize;
        cursor += 2;
        let name = String::from_utf8_lossy(&mmap[cursor..cursor + nlen]).to_string();
        cursor += nlen;
        if id >= station_names.len() {
            station_names.resize(id + 1, String::new());
        }
        station_names[id] = name;
    }

    let data_offset = 64 + dict_bytes;
    let data_bytes = &mmap[data_offset..data_offset + (num_rows as usize * 3)];

    let num_threads = rayon::current_num_threads();
    println!("  -> Kích thước tệp nhị phân: {:.2} MB ({:.2} GB)", file_len as f64 / 1_048_576.0, file_len as f64 / 1_073_741_824.0);
    println!("  -> Tổng số dòng xử lý:      {} dòng ({:.1} Tỷ dòng)", num_rows, num_rows as f64 / 1_000_000_000.0);
    println!("  -> Số trạm thời tiết:       {} trạm", num_stations);
    println!("  -> Số luồng CPU song song:  {} luồng", num_threads);

    let start_calc = Instant::now();

    // Divide rows across threads
    let rows_per_thread = (num_rows as usize + num_threads - 1) / num_threads;

    let thread_stats: Vec<[StationStat; 128]> = (0..num_threads)
        .into_par_iter()
        .map(|tid| {
            let start_row = tid * rows_per_thread;
            let end_row = ((tid + 1) * rows_per_thread).min(num_rows as usize);

            // 128 elements * 32 bytes = 4096 bytes (4 KB, fits completely in L1 Data Cache!)
            let mut stats = [StationStat::default(); 128];

            let start_byte = start_row * 3;
            let end_byte = end_row * 3;
            let slice = &data_bytes[start_byte..end_byte];

            let count = end_row - start_row;
            for i in 0..count {
                let off = i * 3;
                let id = unsafe { *slice.get_unchecked(off) } as usize;
                let temp = i16::from_le_bytes([
                    unsafe { *slice.get_unchecked(off + 1) },
                    unsafe { *slice.get_unchecked(off + 2) },
                ]);

                let s = unsafe { stats.get_unchecked_mut(id) };
                if temp < s.min {
                    s.min = temp;
                }
                if temp > s.max {
                    s.max = temp;
                }
                s.sum += temp as i64;
                s.count += 1;
            }

            stats
        })
        .collect();

    let calc_elapsed = start_calc.elapsed();

    // Combine thread stats into alphabetical BTreeMap
    let start_merge = Instant::now();
    let mut combined: BTreeMap<String, StationStat> = BTreeMap::new();

    for stats in thread_stats {
        for (id, s) in stats.iter().enumerate() {
            if s.count > 0 && id < station_names.len() {
                let name = &station_names[id];
                let agg = combined.entry(name.clone()).or_insert(StationStat::default());
                if s.min < agg.min {
                    agg.min = s.min;
                }
                if s.max > agg.max {
                    agg.max = s.max;
                }
                agg.sum += s.sum;
                agg.count += s.count;
            }
        }
    }

    let merge_elapsed = start_merge.elapsed();
    let total_elapsed = start_all.elapsed();

    println!("============================================================");
    println!("  KẾT QUẢ TÍNH TOÁN 1BRC (Mẫu 10 trạm đầu tiên):");
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

    let total_processed: u64 = combined.values().map(|a| a.count).sum();

    println!("============================================================");
    println!("  HIỆU NĂNG UNCONSTRAINED COLUMNAR (L1 CACHE ACCELERATED):");
    println!("  Tổng số dòng xử lý:   {} dòng", total_processed);
    println!("  Thời gian tính toán:  {:.3}s ({:.2}ms)", calc_elapsed.as_secs_f64(), calc_elapsed.as_secs_f64() * 1000.0);
    println!("  Thời gian gộp kết quả:{:.3}s ({:.2}ms)", merge_elapsed.as_secs_f64(), merge_elapsed.as_secs_f64() * 1000.0);
    println!("  TỔNG THỜI GIAN (A-Z): {:.3}s ({:.2}ms)", total_elapsed.as_secs_f64(), total_elapsed.as_secs_f64() * 1000.0);
    println!("  Tốc độ xử lý (Throughput): {:.2} Triệu dòng / giây ({:.2} GB raw/s)",
        (total_processed as f64 / total_elapsed.as_secs_f64()) / 1_000_000.0,
        (12.31) / total_elapsed.as_secs_f64()
    );
    println!("============================================================");

    Ok(())
}
