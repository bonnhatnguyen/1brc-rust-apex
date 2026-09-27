use rayon::prelude::*;
use std::fs::OpenOptions;
use std::os::windows::fs::FileExt;
use std::time::Instant;

struct Station {
    name: &'static str,
    base_temp: f32,
}

const STATIONS: &[Station] = &[
    Station { name: "Abha", base_temp: 18.0 },
    Station { name: "Abidjan", base_temp: 26.0 },
    Station { name: "Adelaide", base_temp: 17.3 },
    Station { name: "Algiers", base_temp: 18.2 },
    Station { name: "Amsterdam", base_temp: 10.2 },
    Station { name: "Ankara", base_temp: 12.0 },
    Station { name: "Athens", base_temp: 19.2 },
    Station { name: "Auckland", base_temp: 15.2 },
    Station { name: "Bangkok", base_temp: 28.6 },
    Station { name: "Barcelona", base_temp: 18.2 },
    Station { name: "Beijing", base_temp: 12.9 },
    Station { name: "Beirut", base_temp: 20.9 },
    Station { name: "Belgrade", base_temp: 12.5 },
    Station { name: "Berlin", base_temp: 10.3 },
    Station { name: "Bogota", base_temp: 13.5 },
    Station { name: "Boston", base_temp: 10.9 },
    Station { name: "Brasilia", base_temp: 21.4 },
    Station { name: "Brisbane", base_temp: 21.4 },
    Station { name: "Brussels", base_temp: 10.5 },
    Station { name: "Budapest", base_temp: 11.3 },
    Station { name: "Buenos Aires", base_temp: 17.7 },
    Station { name: "Cairo", base_temp: 22.1 },
    Station { name: "Calgary", base_temp: 4.4 },
    Station { name: "Cape Town", base_temp: 17.3 },
    Station { name: "Casablanca", base_temp: 17.6 },
    Station { name: "Chicago", base_temp: 10.8 },
    Station { name: "Copenhagen", base_temp: 9.1 },
    Station { name: "Da Nang", base_temp: 26.5 },
    Station { name: "Dallas", base_temp: 19.0 },
    Station { name: "Damascus", base_temp: 17.0 },
    Station { name: "Delhi", base_temp: 25.0 },
    Station { name: "Denver", base_temp: 10.4 },
    Station { name: "Dhaka", base_temp: 25.9 },
    Station { name: "Dubai", base_temp: 28.2 },
    Station { name: "Dublin", base_temp: 9.8 },
    Station { name: "Edinburgh", base_temp: 9.3 },
    Station { name: "Frankfurt", base_temp: 10.6 },
    Station { name: "Geneva", base_temp: 11.0 },
    Station { name: "Hamburg", base_temp: 9.7 },
    Station { name: "Hanoi", base_temp: 24.2 },
    Station { name: "Helsinki", base_temp: 5.9 },
    Station { name: "Ho Chi Minh City", base_temp: 27.8 },
    Station { name: "Hong Kong", base_temp: 23.3 },
    Station { name: "Honolulu", base_temp: 25.4 },
    Station { name: "Houston", base_temp: 21.4 },
    Station { name: "Istanbul", base_temp: 14.5 },
    Station { name: "Jakarta", base_temp: 27.5 },
    Station { name: "Jerusalem", base_temp: 17.5 },
    Station { name: "Johannesburg", base_temp: 16.0 },
    Station { name: "Kabul", base_temp: 13.0 },
    Station { name: "Karachi", base_temp: 26.0 },
    Station { name: "Kathmandu", base_temp: 18.3 },
    Station { name: "Kyiv", base_temp: 8.4 },
    Station { name: "Kuala Lumpur", base_temp: 27.3 },
    Station { name: "Lagos", base_temp: 26.8 },
    Station { name: "Lahore", base_temp: 24.3 },
    Station { name: "Lima", base_temp: 19.3 },
    Station { name: "Lisbon", base_temp: 17.5 },
    Station { name: "London", base_temp: 11.3 },
    Station { name: "Los Angeles", base_temp: 18.6 },
    Station { name: "Madrid", base_temp: 15.0 },
    Station { name: "Manila", base_temp: 28.4 },
    Station { name: "Melbourne", base_temp: 15.1 },
    Station { name: "Mexico City", base_temp: 16.1 },
    Station { name: "Miami", base_temp: 25.1 },
    Station { name: "Milan", base_temp: 13.0 },
    Station { name: "Montreal", base_temp: 6.8 },
    Station { name: "Moscow", base_temp: 5.8 },
    Station { name: "Mumbai", base_temp: 27.4 },
    Station { name: "Nairobi", base_temp: 17.8 },
    Station { name: "New York", base_temp: 12.7 },
    Station { name: "Oslo", base_temp: 6.4 },
    Station { name: "Paris", base_temp: 12.3 },
    Station { name: "Prague", base_temp: 9.1 },
    Station { name: "Reykjavik", base_temp: 4.8 },
    Station { name: "Rio de Janeiro", base_temp: 24.2 },
    Station { name: "Rome", base_temp: 15.8 },
    Station { name: "San Francisco", base_temp: 14.0 },
    Station { name: "Seoul", base_temp: 12.5 },
    Station { name: "Singapore", base_temp: 27.6 },
    Station { name: "Stockholm", base_temp: 7.0 },
    Station { name: "Sydney", base_temp: 17.7 },
    Station { name: "Taipei", base_temp: 23.0 },
    Station { name: "Tokyo", base_temp: 15.4 },
    Station { name: "Toronto", base_temp: 9.4 },
    Station { name: "Vancouver", base_temp: 10.4 },
    Station { name: "Vienna", base_temp: 10.4 },
    Station { name: "Warsaw", base_temp: 8.5 },
    Station { name: "Washington", base_temp: 14.5 },
    Station { name: "Zurich", base_temp: 9.3 },
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let num_rows: u64 = if args.len() > 1 {
        args[1].parse().unwrap_or(1_000_000_000)
    } else {
        1_000_000_000
    };

    let filename = if args.len() > 2 {
        args[2].clone()
    } else {
        "measurements_1b.txt".to_string()
    };

    println!("============================================================");
    println!("  1BRC MULTI-THREADED PARALLEL DATASET GENERATOR");
    println!("  Target:   {} rows ({:.1} Billion)", num_rows, num_rows as f64 / 1_000_000_000.0);
    println!("  Output:   {}", filename);
    println!("============================================================");

    let start = Instant::now();
    let num_parts = rayon::current_num_threads();
    println!("  -> Số luồng song song: {}", num_parts);

    let rows_per_part = num_rows / num_parts as u64;

    // Generate into multiple part files concurrently, then combine or stream
    let part_names: Vec<String> = (0..num_parts)
        .map(|i| format!("{}.part{}", filename, i))
        .collect();

    part_names.par_iter().enumerate().for_each(|(part_idx, part_file)| {
        let count = if part_idx == num_parts - 1 {
            num_rows - rows_per_part * (num_parts as u64 - 1)
        } else {
            rows_per_part
        };

        use std::io::{BufWriter, Write};
        let file = std::fs::File::create(part_file).expect("create part file");
        let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);

        let mut rng_state: u64 = 0x853c49e6748fea9b ^ ((part_idx as u64 + 1) * 0x9e3779b97f4a7c15);
        let n_stations = STATIONS.len();
        let mut line_buf = String::with_capacity(64);

        for _ in 0..count {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;

            let st_idx = (rng_state as usize) % n_stations;
            let station = &STATIONS[st_idx];

            let delta = ((rng_state >> 32) as i32 % 201) as f32 / 10.0 - 10.0;
            let temp = station.base_temp + delta;

            line_buf.clear();
            use std::fmt::Write as FmtWrite;
            let _ = write!(line_buf, "{};{:.1}\n", station.name, temp);
            let _ = writer.write_all(line_buf.as_bytes());
        }
        let _ = writer.flush();
    });

    println!("  -> Đã tạo xong {} phần song song trong {:.2}s. Đang ghép file...", num_parts, start.elapsed().as_secs_f64());

    // Concatenate parts into single target file
    let merge_start = Instant::now();
    let final_file = std::fs::File::create(&filename)?;
    let mut writer = std::io::BufWriter::with_capacity(32 * 1024 * 1024, final_file);

    for part_file in &part_names {
        use std::io::Read;
        let mut part = std::fs::File::open(part_file)?;
        let mut buf = vec![0u8; 16 * 1024 * 1024];
        loop {
            let n = part.read(&mut buf)?;
            if n == 0 {
                break;
            }
            use std::io::Write;
            writer.write_all(&buf[..n])?;
        }
        let _ = std::fs::remove_file(part_file);
    }
    use std::io::Write;
    writer.flush()?;

    let elapsed = start.elapsed();
    let file_size = std::fs::metadata(&filename)?.len();

    println!("============================================================");
    println!("  HOÀN THÀNH TẠO 1 TỶ DÒNG DỮ LIỆU!");
    println!("  Dung lượng: {:.2} MB ({:.2} GB)", file_size as f64 / 1_048_576.0, file_size as f64 / 1_073_741_824.0);
    println!("  Tổng thời gian: {:.2}s ({:.2}M rows/sec)", elapsed.as_secs_f64(), num_rows as f64 / elapsed.as_secs_f64() / 1_000_000.0);
    println!("============================================================");

    Ok(())
}
