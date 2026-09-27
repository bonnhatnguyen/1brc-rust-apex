use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::time::Instant;

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let input_file = if args.len() > 1 {
        args[1].clone()
    } else {
        "measurements_1b.txt".to_string()
    };

    let base_name = input_file.trim_end_matches(".txt");
    let output_file = format!("{}.col_bin", base_name);

    println!("============================================================");
    println!("  1BRC TO COLUMNAR BINARY CONVERTER (UNCONSTRAINED FORMAT)");
    println!("  Input File:   {}", input_file);
    println!("  Output File:  {}", output_file);
    println!("============================================================");

    let start_all = Instant::now();

    let file = File::open(&input_file)?;
    let file_len = file.metadata()?.len();
    let mmap = unsafe { Mmap::map(&file)? };

    println!("  -> Tệp gốc: {:.2} MB ({:.2} GB)", file_len as f64 / 1_048_576.0, file_len as f64 / 1_073_741_824.0);

    // 1. Discover stations
    let probe_limit = (file_len as usize).min(10 * 1024 * 1024);
    let mut station_map: HashMap<String, u8> = HashMap::new();
    let mut station_list: Vec<String> = Vec::new();

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

    println!("  -> Đã nhận diện: {} trạm thời tiết", station_list.len());

    // 2. Open output file and write header placeholder
    let out = File::create(&output_file)?;
    let mut writer = BufWriter::with_capacity(32 * 1024 * 1024, out);

    // Header (64 bytes):
    // Magic: "1BRCCOL1" (8)
    // num_stations: u32 (4)
    // dict_bytes: u32 (4)
    // num_rows: u64 (8)
    // reserved: [u8; 40]
    writer.write_all(b"1BRCCOL1")?;
    writer.write_all(&(station_list.len() as u32).to_le_bytes())?;
    writer.write_all(&0u32.to_le_bytes())?; // dict_bytes placeholder
    writer.write_all(&0u64.to_le_bytes())?; // num_rows placeholder
    writer.write_all(&[0u8; 40])?;

    // Write dictionary: [id: u8, len: u16, bytes]
    let dict_start = Instant::now();
    let mut dict_bytes_written = 0u32;
    for (id, name) in station_list.iter().enumerate() {
        writer.write_all(&(id as u8).to_le_bytes())?;
        let nb = name.as_bytes();
        writer.write_all(&(nb.len() as u16).to_le_bytes())?;
        writer.write_all(nb)?;
        dict_bytes_written += 1 + 2 + nb.len() as u32;
    }

    // 3. Partition file for multi-threaded conversion
    let num_threads = rayon::current_num_threads();
    println!("  -> Bắt đầu chuyển đổi đa luồng ({} luồng)...", num_threads);

    let chunk_size = file_len / num_threads as u64;
    let mut chunk_boundaries = Vec::with_capacity(num_threads);
    let mut current_offset: usize = 0;

    for i in 0..num_threads {
        let start = current_offset;
        let mut end = if i == num_threads - 1 {
            file_len as usize
        } else {
            ((i + 1) as u64 * chunk_size) as usize
        };

        if end < file_len as usize {
            while end < file_len as usize && mmap[end] != b'\n' {
                end += 1;
            }
            if end < file_len as usize {
                end += 1;
            }
        }

        chunk_boundaries.push((start, end));
        current_offset = end;
    }

    // Convert chunks in parallel into packed 3-byte vectors
    let start_conv = Instant::now();
    let thread_buffers: Vec<Vec<u8>> = chunk_boundaries
        .into_par_iter()
        .map(|(start, end)| {
            let chunk = &mmap[start..end];
            let mut out_buf = Vec::with_capacity((end - start) / 4); // ~3 bytes per line of ~12 bytes = 25%

            let mut cur = 0;
            let clen = chunk.len();

            while cur < clen {
                let nstart = cur;
                while cur < clen && chunk[cur] != b';' {
                    cur += 1;
                }
                if cur >= clen {
                    break;
                }
                let name = &chunk[nstart..cur];
                cur += 1;

                let tstart = cur;
                while cur < clen && chunk[cur] != b'\n' {
                    cur += 1;
                }
                let mut tend = cur;
                if tend > tstart && chunk[tend - 1] == b'\r' {
                    tend -= 1;
                }
                let tbytes = &chunk[tstart..tend];
                cur += 1;

                if let Ok(name_str) = std::str::from_utf8(name) {
                    if let Some(&id) = station_map.get(name_str) {
                        let temp = parse_temp_fast(tbytes);
                        out_buf.push(id);
                        out_buf.extend_from_slice(&temp.to_le_bytes());
                    }
                }
            }

            out_buf
        })
        .collect();

    println!("  -> Chuyển đổi nhị phân xong trong {:.2}s. Đang ghi file...", start_conv.elapsed().as_secs_f64());

    // Write all buffers
    let mut total_rows = 0u64;
    for buf in thread_buffers {
        let rows_in_buf = (buf.len() / 3) as u64;
        total_rows += rows_in_buf;
        writer.write_all(&buf)?;
    }
    writer.flush()?;

    // Update Header with final counts
    let mut update_file = File::options().write(true).open(&output_file)?;
    update_file.seek(SeekFrom::Start(12))?;
    update_file.write_all(&dict_bytes_written.to_le_bytes())?;
    update_file.write_all(&total_rows.to_le_bytes())?;
    update_file.flush()?;

    let final_size = std::fs::metadata(&output_file)?.len();
    let total_elapsed = start_all.elapsed();

    println!("============================================================");
    println!("  HOÀN THÀNH TẠO FILE COLUMNAR BINARY!");
    println!("  Tổng số dòng:       {} dòng", total_rows);
    println!("  Dung lượng gốc:     {:.2} MB ({:.2} GB)", file_len as f64 / 1_048_576.0, file_len as f64 / 1_073_741_824.0);
    println!("  Dung lượng Columnar:{:.2} MB ({:.2} GB) [Tỷ lệ: {:.1}%]",
        final_size as f64 / 1_048_576.0, final_size as f64 / 1_073_741_824.0,
        (final_size as f64 / file_len as f64) * 100.0
    );
    println!("  Tiết kiệm ổ đĩa:    {:.2} GB ({:.1}%)",
        (file_len - final_size) as f64 / 1_073_741_824.0,
        (1.0 - final_size as f64 / file_len as f64) * 100.0
    );
    println!("  Tổng thời gian tạo: {:.2}s", total_elapsed.as_secs_f64());
    println!("============================================================");

    Ok(())
}
