//! Packaging benchmark: `cargo bench --bench package`.
//!
//! Generates a synthetic build context (or uses `RUNWAY_BENCH_DIR`) and times
//! the hash-only scan used by `plan` and the parallel compression used by
//! `deploy` when an upload is needed.

use runway::build::package;
use std::io::Write;
use std::time::Instant;

fn generate(root: &std::path::Path) {
    // ~8k source files and a 32 MiB incompressible asset (~66 MiB total).
    for i in 0..8000 {
        let dir = root.join(format!("src/m{}/p{}", i % 100, i % 13));
        std::fs::create_dir_all(&dir).unwrap();
        let body = format!("def f{i}(x):\n    return x * {i}  # some code line\n").repeat(80);
        std::fs::write(dir.join(format!("f{i}.py")), body).unwrap();
    }
    let mut state = 0x9e3779b97f4a7c15u64;
    let mut f = std::fs::File::create(root.join("model.bin")).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    for _ in 0..32 {
        for b in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = state as u8;
        }
        f.write_all(&buf).unwrap();
    }
    std::fs::write(root.join("Dockerfile"), "FROM scratch\nCOPY . /app\n").unwrap();
}

fn main() {
    let tmp;
    let root = match std::env::var_os("RUNWAY_BENCH_DIR") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            tmp = tempfile::tempdir().unwrap();
            generate(tmp.path());
            tmp.path().to_path_buf()
        }
    };
    let runs = 5;
    let mut scan_ms = Vec::new();
    let mut compress_ms = Vec::new();
    let mut last = None;
    for _ in 0..runs {
        let t = Instant::now();
        let strategy = runway::config::BuildStrategy::Dockerfile {
            path: "Dockerfile".into(),
        };
        let m = package::scan(&root, &strategy, &[]).unwrap();
        scan_ms.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        let a = package::compress(&m).unwrap();
        compress_ms.push(t.elapsed().as_secs_f64() * 1e3);
        last = Some((m, a.len));
    }
    let (m, gz) = last.unwrap();
    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let mib = m.tar_bytes as f64 / (1024.0 * 1024.0);
    let s = median(&mut scan_ms);
    let c = median(&mut compress_ms);
    println!(
        "context: {} files, {mib:.1} MiB uncompressed, {:.1} MiB gzipped",
        m.files,
        gz as f64 / 1048576.0
    );
    println!(
        "scan (hash only):          {s:8.1} ms  ({:.0} MiB/s)",
        mib / (s / 1e3)
    );
    println!(
        "compress (parallel gzip):  {c:8.1} ms  ({:.0} MiB/s)",
        mib / (c / 1e3)
    );
}
